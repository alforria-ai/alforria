//! Server-held browser login. Starting a login binds the loopback and hands
//! back the console URL; the flow then completes from the loopback redirect
//! or from text the user pastes (a browser on another machine can't reach
//! this host's loopback), whichever arrives first. Completion exchanges the
//! code, mints this device's key, writes the session sidecar and hands the
//! key to the caller's sink — even when nobody is waiting on the flow.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::auth::{parse_manual_code, CallbackServer, Endpoints, FullApiKey, Pkce};

/// Persists a freshly minted key (the auth.json `libertai` entry).
pub type KeySink = Box<dyn Fn(&FullApiKey) -> Result<(), String> + Send + Sync>;

/// One pending browser login. Dropping it releases the loopback port.
pub struct PendingLogin {
    url: String,
    redirect_uri: String,
    inner: Arc<Inner>,
}

struct Inner {
    endpoints: Endpoints,
    pkce: Pkce,
    sink: KeySink,
    /// Stops the loopback listener.
    cancel: AtomicBool,
    /// `None` while pending; the first resolution wins.
    outcome: Mutex<Option<Result<(), String>>>,
    resolved: Condvar,
    /// Serializes completion attempts, so the loopback and a paste never
    /// both exchange.
    completing: Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PendingLogin {
    /// Bind the loopback, build the console authorize URL and start
    /// listening for the redirect for up to `timeout`.
    pub fn start(
        endpoints: Endpoints,
        client: &str,
        timeout: Duration,
        sink: KeySink,
    ) -> std::io::Result<PendingLogin> {
        let server = CallbackServer::bind()?;
        let pkce = Pkce::generate();
        let redirect_uri = server.redirect_uri();
        let url = endpoints.authorize_url(&pkce, client, &redirect_uri);
        let inner = Arc::new(Inner {
            endpoints,
            pkce,
            sink,
            cancel: AtomicBool::new(false),
            outcome: Mutex::new(None),
            resolved: Condvar::new(),
            completing: Mutex::new(()),
        });
        let listener = inner.clone();
        std::thread::Builder::new()
            .name("libertai-login".to_string())
            .spawn(move || {
                let _ =
                    std::panic::catch_unwind(AssertUnwindSafe(|| listener.listen(server, timeout)));
                // Never leave a waiter hanging, even if the listener panicked.
                listener.resolve(Err("sign-in listener stopped".to_string()));
            })?;
        Ok(PendingLogin {
            url,
            redirect_uri,
            inner,
        })
    }

    /// The console authorize URL to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The loopback address the console redirects to.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Block until the flow resolves — loopback redirect, accepted paste,
    /// timeout or cancellation. The listener thread always resolves the
    /// flow when it exits, which bounds the wait.
    pub fn wait(&self) -> Result<(), String> {
        let mut outcome = lock(&self.inner.outcome);
        loop {
            if let Some(result) = outcome.as_ref() {
                return result.clone();
            }
            outcome = self
                .inner
                .resolved
                .wait(outcome)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Complete the flow with text the user pasted: a bare code,
    /// `code=…&state=…`, or the full redirect URL. A rejected paste (no
    /// code, state mismatch, failed exchange) leaves the flow pending, so
    /// the loopback or a corrected paste can still finish it.
    pub fn submit(&self, pasted: &str) -> Result<(), String> {
        let (code, state) = parse_manual_code(pasted)
            .ok_or_else(|| "could not find a login code in the pasted text".to_string())?;
        // A bare code carries no state; the PKCE verifier alone guards the
        // exchange.
        if state
            .as_deref()
            .is_some_and(|state| state != self.inner.pkce.state)
        {
            return Err("login state mismatch".to_string());
        }
        let _completing = lock(&self.inner.completing);
        if let Some(result) = lock(&self.inner.outcome).clone() {
            return result;
        }
        self.inner.complete(&code)?;
        self.inner.cancel.store(true, Ordering::SeqCst);
        self.inner.resolve(Ok(()));
        Ok(())
    }

    /// Stop listening and fail every waiter — a newer flow replaced this
    /// one.
    pub fn cancel(&self) {
        self.inner.cancel.store(true, Ordering::SeqCst);
        self.inner.resolve(Err("sign-in cancelled".to_string()));
    }

    pub fn is_resolved(&self) -> bool {
        lock(&self.inner.outcome).is_some()
    }
}

impl Drop for PendingLogin {
    fn drop(&mut self) {
        self.inner.cancel.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn listen(&self, server: CallbackServer, timeout: Duration) {
        let callback = server.wait_cancellable(timeout, &self.cancel);
        // Release the port before the (slow) exchange.
        drop(server);
        let callback = match callback {
            Ok(callback) => callback,
            Err(err) => return self.resolve(Err(err)),
        };
        let _completing = lock(&self.completing);
        if lock(&self.outcome).is_some() {
            return;
        }
        if callback.state != self.pkce.state {
            return self.resolve(Err("login state mismatch".to_string()));
        }
        let result = self.complete(&callback.code);
        self.resolve(result);
    }

    fn complete(&self, code: &str) -> Result<(), String> {
        let key = self.endpoints.complete_login(code, &self.pkce.verifier)?;
        (self.sink)(&key)
    }

    /// First resolution wins; wakes every waiter.
    fn resolve(&self, result: Result<(), String>) {
        let mut outcome = lock(&self.outcome);
        if outcome.is_none() {
            *outcome = Some(result);
            self.resolved.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn endpoints(dir: &std::path::Path) -> Endpoints {
        Endpoints {
            // Nothing listens here: any exchange attempt fails fast.
            account: "http://127.0.0.1:9".to_string(),
            console: "https://console.example".to_string(),
            session_file: dir.join("libertai-auth.json"),
        }
    }

    fn start(dir: &std::path::Path) -> PendingLogin {
        PendingLogin::start(
            endpoints(dir),
            "Alforria",
            Duration::from_secs(30),
            Box::new(|_: &FullApiKey| -> Result<(), String> {
                panic!("no key is minted in these tests")
            }),
        )
        .unwrap()
    }

    fn state_of(login: &PendingLogin) -> String {
        login.inner.pkce.state.clone()
    }

    fn port_of(login: &PendingLogin) -> u16 {
        login
            .redirect_uri()
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/callback")
            .parse()
            .unwrap()
    }

    #[test]
    fn url_targets_the_console_with_a_loopback_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let login = start(dir.path());
        assert!(login.url().starts_with("https://console.example/cli?"));
        assert!(login.url().contains("client=Alforria"));
        assert!(login.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(login.url().contains(&format!("state={}", state_of(&login))));
    }

    #[test]
    fn rejected_pastes_leave_the_flow_pending() {
        let dir = tempfile::tempdir().unwrap();
        let login = start(dir.path());
        assert!(login.submit("   ").is_err());
        assert_eq!(
            login.submit("code=abc&state=not-the-state").unwrap_err(),
            "login state mismatch"
        );
        // The exchange against the dead account base fails, too.
        assert!(login
            .submit(&format!("code=abc&state={}", state_of(&login)))
            .is_err());
        assert!(!login.is_resolved());
    }

    #[test]
    fn cancel_wakes_waiters_and_releases_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let login = Arc::new(start(dir.path()));
        let port = port_of(&login);
        let waiter = {
            let login = login.clone();
            std::thread::spawn(move || login.wait())
        };
        login.cancel();
        assert_eq!(waiter.join().unwrap().unwrap_err(), "sign-in cancelled");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while TcpStream::connect(("127.0.0.1", port)).is_ok() {
            assert!(std::time::Instant::now() < deadline, "port still bound");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn loopback_state_mismatch_fails_the_flow() {
        let dir = tempfile::tempdir().unwrap();
        let login = start(dir.path());
        let mut stream = TcpStream::connect(("127.0.0.1", port_of(&login))).unwrap();
        stream
            .write_all(b"GET /callback?code=abc&state=forged HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert_eq!(login.wait().unwrap_err(), "login state mismatch");
        // A later paste reports the settled failure.
        assert!(login
            .submit(&format!("code=abc&state={}", state_of(&login)))
            .is_err());
    }
}

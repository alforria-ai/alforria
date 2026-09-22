//! Bash command-prefix arities — port of `permission/arity.ts`.
//!
//! The table maps a command-prefix string to the number of tokens that
//! define the "human-understandable" command. Flags never count, only
//! subcommands do; the longest matching prefix wins. Ported verbatim from
//! arity.ts:24-161.

use std::collections::HashMap;

/// `prefix(tokens)` (arity.ts:1-9): longest matching prefix wins; a missing
/// entry keeps only the first token; an empty input yields nothing.
pub fn prefix<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    let table = arity();
    for len in (1..=tokens.len()).rev() {
        let joined = tokens[..len].join(" ");
        if let Some(arity) = table.get(joined.as_str()) {
            return tokens[..(*arity as usize).min(tokens.len())].to_vec();
        }
    }
    if tokens.is_empty() {
        return Vec::new();
    }
    tokens[..1].to_vec()
}

fn arity() -> &'static HashMap<&'static str, u32> {
    use std::sync::OnceLock;
    static ARITY: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
    ARITY.get_or_init(|| {
        let entries: &[(&'static str, u32)] = &[
            ("cat", 1),
            ("cd", 1),
            ("chmod", 1),
            ("chown", 1),
            ("cp", 1),
            ("echo", 1),
            ("env", 1),
            ("export", 1),
            ("grep", 1),
            ("kill", 1),
            ("killall", 1),
            ("ln", 1),
            ("ls", 1),
            ("mkdir", 1),
            ("mv", 1),
            ("ps", 1),
            ("pwd", 1),
            ("rm", 1),
            ("rmdir", 1),
            ("sleep", 1),
            ("source", 1),
            ("tail", 1),
            ("touch", 1),
            ("unset", 1),
            ("which", 1),
            ("aws", 3),
            ("az", 3),
            ("bazel", 2),
            ("brew", 2),
            ("bun", 2),
            ("bun run", 3),
            ("bun x", 3),
            ("cargo", 2),
            ("cargo add", 3),
            ("cargo run", 3),
            ("cdk", 2),
            ("cf", 2),
            ("cmake", 2),
            ("composer", 2),
            ("consul", 2),
            ("consul kv", 3),
            ("crictl", 2),
            ("deno", 2),
            ("deno task", 3),
            ("doctl", 3),
            ("docker", 2),
            ("docker builder", 3),
            ("docker compose", 3),
            ("docker container", 3),
            ("docker image", 3),
            ("docker network", 3),
            ("docker volume", 3),
            ("eksctl", 2),
            ("eksctl create", 3),
            ("firebase", 2),
            ("flyctl", 2),
            ("gcloud", 3),
            ("gh", 3),
            ("git", 2),
            ("git config", 3),
            ("git remote", 3),
            ("git stash", 3),
            ("go", 2),
            ("gradle", 2),
            ("helm", 2),
            ("heroku", 2),
            ("hugo", 2),
            ("ip", 2),
            ("ip addr", 3),
            ("ip link", 3),
            ("ip netns", 3),
            ("ip route", 3),
            ("kind", 2),
            ("kind create", 3),
            ("kubectl", 2),
            ("kubectl kustomize", 3),
            ("kubectl rollout", 3),
            ("kustomize", 2),
            ("make", 2),
            ("mc", 2),
            ("mc admin", 3),
            ("minikube", 2),
            ("mongosh", 2),
            ("mysql", 2),
            ("mvn", 2),
            ("ng", 2),
            ("npm", 2),
            ("npm exec", 3),
            ("npm init", 3),
            ("npm run", 3),
            ("npm view", 3),
            ("nvm", 2),
            ("nx", 2),
            ("openssl", 2),
            ("openssl req", 3),
            ("openssl x509", 3),
            ("pip", 2),
            ("pipenv", 2),
            ("pnpm", 2),
            ("pnpm dlx", 3),
            ("pnpm exec", 3),
            ("pnpm run", 3),
            ("poetry", 2),
            ("podman", 2),
            ("podman container", 3),
            ("podman image", 3),
            ("psql", 2),
            ("pulumi", 2),
            ("pulumi stack", 3),
            ("pyenv", 2),
            ("python", 2),
            ("rake", 2),
            ("rbenv", 2),
            ("redis-cli", 2),
            ("rustup", 2),
            ("serverless", 2),
            ("sfdx", 3),
            ("skaffold", 2),
            ("sls", 2),
            ("sst", 2),
            ("swift", 2),
            ("systemctl", 2),
            ("terraform", 2),
            ("terraform workspace", 3),
            ("tmux", 2),
            ("turbo", 2),
            ("ufw", 2),
            ("vault", 2),
            ("vault auth", 3),
            ("vault kv", 3),
            ("vercel", 2),
            ("volta", 2),
            ("wp", 2),
            ("yarn", 2),
            ("yarn dlx", 3),
            ("yarn run", 3),
        ];
        let mut map = HashMap::with_capacity(entries.len());
        for (key, value) in entries {
            map.insert(*key, *value);
        }
        map
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arity_examples_from_the_doc_comment() {
        assert_eq!(prefix(&["touch", "foo.txt"]), vec!["touch"]);
        assert_eq!(
            prefix(&["git", "checkout", "main"]),
            vec!["git", "checkout"]
        );
        assert_eq!(prefix(&["npm", "install"]), vec!["npm", "install"]);
        assert_eq!(prefix(&["npm", "run", "dev"]), vec!["npm", "run", "dev"]);
        assert_eq!(
            prefix(&["python", "script.py"]),
            vec!["python", "script.py"]
        );
    }

    #[test]
    fn flags_never_count() {
        assert_eq!(
            prefix(&["git", "checkout", "--foo", "main"]),
            vec!["git", "checkout"]
        );
        // TS: tokens.slice(0, arity) — the slice may include flags.
        assert_eq!(
            prefix(&["cargo", "add", "--verbose", "tokio"]),
            vec!["cargo", "add", "--verbose"]
        );
    }

    #[test]
    fn longest_prefix_wins() {
        assert_eq!(
            prefix(&["docker", "compose", "up", "-d"]),
            vec!["docker", "compose", "up"]
        );
        assert_eq!(prefix(&["docker", "run"]), vec!["docker", "run"]);
        assert_eq!(prefix(&["docker"]), vec!["docker"]);
    }

    #[test]
    fn unknown_and_empty() {
        assert_eq!(prefix(&["foobar", "x"]), vec!["foobar"]);
        assert!(prefix(&[]).is_empty());
    }
}

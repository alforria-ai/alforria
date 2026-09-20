//! Live LibertAI scenarios (spec E2E §3). Every test is gated behind
//! `OPENCODE_E2E_LIVE=1` — without the env they return in milliseconds
//! and the default suite is unaffected. Model matrix (§3.4): the cheap
//! tier drives the full B-scenario set; the quality tiers smoke only
//! (file mutation, read-answer, structured). B8 (doom-loop) is
//! deliberately not run live — models don't deterministically repeat
//! calls.

mod e2e_agent {
    mod live {

        use crate::backend::{LibertaiBackend, LiveModel};
        use crate::scenarios;

        fn gated() -> bool {
            std::env::var("OPENCODE_E2E_LIVE").ok().as_deref() == Some("1")
        }

        /// The relaxation retry policy (spec E2E §3.3): a retryable live
        /// mismatch gets one whole-scenario retry; a second failure
        /// propagates with the captured run output in the panic message.
        async fn retry<F, Fut>(mut scenario: F)
        where
            F: FnMut() -> Fut,
            Fut: std::future::Future<Output = ()> + Send + 'static,
        {
            for attempt in 0..2 {
                match tokio::spawn(scenario()).await {
                    Ok(()) => return,
                    Err(failure) if attempt == 1 => std::panic::resume_unwind(failure.into_panic()),
                    Err(_flaky) => continue,
                }
            }
        }

        // -------------------------------------------------------------------
        // Cheap tier — qwen3.5-4b, all live scenarios
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn b1_file_mutation_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::a1_file_mutation(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b2_read_answer_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::a2_multi_step(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b3_permission_ask_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::b3_permission_ask(&backend).await;
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b4_subagent_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::b4_subagent(&backend).await;
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b5_cancel_reprompt_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::b5_cancel_reprompt(&backend).await;
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b6_structured_output_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::b6_structured_output(&backend).await;
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b7_export_round_trip_qwen() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::Cheap);
                async move {
                    scenarios::b7_session_export_round_trip(&backend);
                }
            })
            .await;
        }

        // -------------------------------------------------------------------
        // Quality tier — smoke only (§3.4): glm-5.3 + deepseek-v4.1-flash
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn b1_file_mutation_glm() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityGlm);
                async move {
                    scenarios::a1_file_mutation(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b2_read_answer_glm() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityGlm);
                async move {
                    scenarios::a2_multi_step(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b6_structured_output_glm() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityGlm);
                async move {
                    scenarios::b6_structured_output(&backend).await;
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b1_file_mutation_deepseek() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityDeepseek);
                async move {
                    scenarios::a1_file_mutation(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b2_read_answer_deepseek() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityDeepseek);
                async move {
                    scenarios::a2_multi_step(&backend);
                }
            })
            .await;
        }

        #[tokio::test]
        async fn b6_structured_output_deepseek() {
            if !gated() {
                return;
            }
            retry(|| {
                let backend = LibertaiBackend::new(LiveModel::QualityDeepseek);
                async move {
                    scenarios::b6_structured_output(&backend).await;
                }
            })
            .await;
        }
    }
}

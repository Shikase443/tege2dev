use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::MissedTickBehavior;

#[derive(Clone, Copy)]
enum Health {
    Healthy,
    Warning,
    Unhealthy,
}

impl Health {
    fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    health: Health,
    successes: u8,
    samples: u8,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            health: Health::Unhealthy,
            successes: 0,
            samples: 0,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct LlmHealth {
    snapshot: Arc<RwLock<Snapshot>>,
}

impl LlmHealth {
    pub(crate) fn start(base_url: &str) -> Self {
        let state = Self::default();
        let address = reqwest::Url::parse(base_url).ok().and_then(|url| {
            Some((url.host_str()?.to_owned(), url.port_or_known_default()?))
        });
        let snapshot = Arc::clone(&state.snapshot);
        tokio::spawn(async move {
            let mut probes = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(3),
                Duration::from_secs(3),
            );
            probes.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut evaluations = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
            );
            evaluations.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut successes = 0_u8;
            let mut samples = 0_u8;
            loop {
                tokio::select! {
                    _ = probes.tick() => {
                        let connected = match &address {
                            Some((host, port)) => tokio::time::timeout(
                                Duration::from_secs(1),
                                TcpStream::connect((host.as_str(), *port)),
                            ).await.is_ok_and(|result| result.is_ok()),
                            None => false,
                        };
                        samples = samples.saturating_add(1);
                        successes = successes.saturating_add(u8::from(connected));
                    }
                    _ = evaluations.tick() => {
                        let health = match (successes, samples) {
                            (0, _) => Health::Unhealthy,
                            (successes, samples) if successes == samples => Health::Healthy,
                            _ => Health::Warning,
                        };
                        *snapshot.write().await = Snapshot { health, successes, samples };
                        successes = 0;
                        samples = 0;
                    }
                }
            }
        });
        state
    }

    pub(crate) async fn response(&self) -> Value {
        let snapshot = *self.snapshot.read().await;
        json!({
            "status": snapshot.health.as_str(),
            "successCount": snapshot.successes,
            "sampleCount": snapshot.samples,
        })
    }
}

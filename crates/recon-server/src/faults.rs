//! Pause points for the kill drills (`kill-mid-import`). Built only with the
//! `fault-injection` feature; otherwise every point is a no-op and `RECON_FAULT` refuses to
//! start the server (config).
//!
//! `RECON_FAULT=<point>` names one point (`staged_batch:<n>`, `before_flip`,
//! `flip_lines_inserted`, `flip_before_commit`, `after_commit` for imports; `match_snapshot`,
//! `match_chunk:<n>`, `match_before_finish`, `match_finished` for the matching worker). When
//! the server reaches it, it creates the file `RECON_FAULT_MARKER` and waits forever,
//! holding whatever it holds (inside the flip: an open transaction), so the drill can
//! `SIGKILL` it at exactly that point.

use recon_db::imports::{FlipHook, FlipPoint};

/// A point in the import path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// The n-th staged batch (1-based) has committed.
    StagedBatch(u32),
    /// Parsing finished and everything is staged; the flip has not begun.
    BeforeFlip,
    /// Inside the flip: lines inserted, nothing committed.
    FlipLinesInserted,
    /// Inside the flip: everything written, the commit is next.
    FlipBeforeCommit,
    /// The flip committed; the response has not been sent.
    AfterCommit,
    /// A matching job's snapshot is pinned; no outcome is committed yet.
    MatchSnapshot,
    /// The n-th matching chunk (1-based) has committed; the worker continues.
    MatchChunk(u32),
    /// Every chunk committed; the job's `complete` update has not run.
    MatchBeforeFinish,
    /// The job row is `complete`; the worker has not exited.
    MatchFinished,
    /// The engine stretch started; it never returns (the deadline drill's hung engine).
    MatchEngine,
}

impl FaultPoint {
    /// Parses `RECON_FAULT`'s syntax.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "before_flip" => Self::BeforeFlip,
            "flip_lines_inserted" => Self::FlipLinesInserted,
            "flip_before_commit" => Self::FlipBeforeCommit,
            "after_commit" => Self::AfterCommit,
            "match_snapshot" => Self::MatchSnapshot,
            "match_before_finish" => Self::MatchBeforeFinish,
            "match_finished" => Self::MatchFinished,
            "match_engine" => Self::MatchEngine,
            other => other
                .strip_prefix("staged_batch:")
                .and_then(|n| n.parse().ok())
                .map(Self::StagedBatch)
                .or_else(|| {
                    other
                        .strip_prefix("match_chunk:")
                        .and_then(|n| n.parse().ok())
                        .map(Self::MatchChunk)
                })?,
        })
    }
}

/// The configured fault, if any.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    #[cfg(feature = "fault-injection")]
    target: Option<(FaultPoint, std::path::PathBuf)>,
}

impl Faults {
    /// Reads `RECON_FAULT` and `RECON_FAULT_MARKER` (fault-injection builds only).
    pub fn from_env() -> Result<Self, String> {
        #[cfg(feature = "fault-injection")]
        {
            let Ok(point) = std::env::var("RECON_FAULT") else {
                return Ok(Self::default());
            };
            let point = FaultPoint::parse(&point)
                .ok_or_else(|| format!("unknown RECON_FAULT {point:?}"))?;
            let marker = std::env::var("RECON_FAULT_MARKER")
                .map_err(|_| "RECON_FAULT_MARKER is required with RECON_FAULT".to_owned())?;
            Ok(Self {
                target: Some((point, marker.into())),
            })
        }
        #[cfg(not(feature = "fault-injection"))]
        Ok(Self::default())
    }

    /// Called at each point; pauses forever at the configured one.
    #[cfg_attr(
        not(feature = "fault-injection"),
        expect(clippy::unused_async, reason = "a no-op without fault injection")
    )]
    pub async fn reached(&self, point: FaultPoint) {
        #[cfg(feature = "fault-injection")]
        if let Some((target, marker)) = &self.target
            && *target == point
        {
            tracing::warn!(?point, "fault point reached; pausing for the drill");
            let _ = tokio::fs::write(marker, b"reached").await;
            std::future::pending::<()>().await;
        }
        #[cfg(not(feature = "fault-injection"))]
        let _ = point;
    }

    /// Called inside `spawn_blocking` where nothing may await: same marker, then the
    /// thread sleeps forever (the blocking-pool thread leaks until the process ends).
    #[cfg(feature = "fault-injection")]
    pub fn reached_blocking(&self, point: FaultPoint) {
        if let Some((target, marker)) = &self.target
            && *target == point
        {
            tracing::warn!(?point, "fault point reached; pausing for the drill");
            let _ = std::fs::write(marker, b"reached");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3_600));
            }
        }
    }

    /// The no-fault build's `reached_blocking` is a no-op.
    #[cfg(not(feature = "fault-injection"))]
    pub fn reached_blocking(&self, point: FaultPoint) {
        let _ = point;
    }
}

/// The flip's hook: maps the flip's points to fault points.
#[derive(Debug, Clone, Copy)]
pub struct FaultHook<'a>(pub &'a Faults);

impl FlipHook for FaultHook<'_> {
    async fn reached(&self, point: FlipPoint) {
        let point = match point {
            FlipPoint::LinesInserted => FaultPoint::FlipLinesInserted,
            FlipPoint::BeforeCommit => FaultPoint::FlipBeforeCommit,
        };
        self.0.reached(point).await;
    }
}

#[cfg(test)]
mod tests {
    use super::FaultPoint;

    #[test]
    fn points_parse() {
        assert_eq!(
            FaultPoint::parse("staged_batch:7"),
            Some(FaultPoint::StagedBatch(7))
        );
        assert_eq!(
            FaultPoint::parse("flip_before_commit"),
            Some(FaultPoint::FlipBeforeCommit)
        );
        assert_eq!(FaultPoint::parse("staged_batch:x"), None);
        assert_eq!(FaultPoint::parse("anything"), None);
        assert_eq!(
            FaultPoint::parse("match_snapshot"),
            Some(FaultPoint::MatchSnapshot)
        );
        assert_eq!(
            FaultPoint::parse("match_chunk:12"),
            Some(FaultPoint::MatchChunk(12))
        );
        assert_eq!(FaultPoint::parse("match_chunk:x"), None);
    }
}

//! Failpoints: the named interruptible boundaries and the injectors that fire them.

use super::*;

// ---------------------------------------------------------------------------
// Failpoints
// ---------------------------------------------------------------------------

/// Every interruptible boundary in the lifecycle, named for the step it precedes.
///
/// A fault raised at a point means "the step named by this variant never happened", which is
/// exactly the window a crash can land in. Compensation variants fire *during* the rollback of an
/// earlier fault, so a test can prove the double-failure path too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FaultPoint {
    /// Before the staging body is written. [lib/store.js:217], [lib/store.js:263-267]
    PublishStageWrite,
    /// After a publish staging body is written, before its file durability barrier.
    PublishStageFileSync,
    /// After a publish staging body is file-durable, before its parent-directory barrier.
    PublishStageDirectorySync,
    /// After staging, before `INSERT INTO artifacts`. [lib/store.js:218]
    PublishInsert,
    /// After the insert, before `rename(staging → final)`. [lib/store.js:220]
    PublishRename,
    /// After the rename, before the metadata read-back.
    PublishComplete,
    /// Kernel publish rename completed; fail before parent directory sync.
    PublishRenameBarrier,
    /// Compensation: before the inserted artifact row is deleted. [lib/store.js:223],
    /// [lib/store.js:276]
    PublishDeleteRow,
    /// Before the replacement body is staged. [lib/store.js:374-384]
    UpdateStageWrite,
    /// After an update staging body is written, before its file durability barrier.
    UpdateStageFileSync,
    /// After an update staging body is file-durable, before its parent-directory barrier.
    UpdateStageDirectorySync,
    /// After staging, before the metadata transaction opens. [lib/store.js:406]
    UpdateCommit,
    /// Inside the metadata transaction, after the guarded UPDATE and the revision row, before
    /// the commit. [lib/store.js:412-414]
    UpdateCommitTransaction,
    /// After the metadata commit, before the outgoing body is snapshotted. [lib/store.js:424]
    UpdateSnapshot,
    /// After a metadata-only history copy is written, before its file durability barrier.
    UpdateSnapshotFileSync,
    /// After the snapshot, before the staged body is swapped in. [lib/store.js:426]
    UpdateSwap,
    /// Kernel update install rename completed; fail before parent directory sync.
    UpdateSwapBarrier,
    /// Kernel history snapshot rename completed; fail before parent directory sync.
    UpdateSnapshotBarrier,
    /// While creating the `.history/<artifact>` chain, before its first directory barrier.
    UpdateHistoryDirectorySync,
    /// After the swap, before history pruning. [lib/store.js:438]
    UpdatePrune,
    /// Compensation: before the snapshotted body is moved back. [lib/store.js:430]
    UpdateRestoreSnapshot,
    /// Compensation: before the revision row is dropped and metadata reverted.
    /// [lib/store.js:432-435]
    UpdateRevertMetadata,
    /// Compensation: before an owned metadata-only snapshot temporary is durably removed.
    UpdateSnapshotTempRemove,
    /// Kernel snapshot temporary removal completed; fail before its parent-directory barrier.
    UpdateSnapshotTempRemoveBarrier,
    /// Before the live body is renamed into trash. [lib/store.js:503-509]
    DeleteTrashRename,
    /// Kernel delete-to-trash rename completed; fail before parent directory sync.
    DeleteTrashRenameBarrier,
    /// After the body reached trash, before `DELETE FROM artifacts`. [lib/store.js:632]
    DeleteRow,
    /// After the row is gone, before the trashed body is removed. [lib/store.js:637]
    DeleteTrashRemove,
    /// After trash removal, before the history directory is removed. [lib/store.js:638]
    DeleteHistoryRemove,
    /// Compensation: before the trashed body is moved back. [lib/store.js:635], [lib/store.js:642]
    DeleteRestoreBody,
    /// Before the re-tenant transaction runs. [lib/store.js:469-477]
    MoveTransaction,
    /// Before a `set_category` / `set_hidden` write.
    MetadataWrite,
    /// Reconciliation: before an interrupted body is renamed to its final path.
    /// [lib/store.js:702], [lib/store.js:711]
    ReconcileRecover,
    /// Reconciliation: before an unreferenced transient path is removed. [lib/store.js:714]
    ReconcileDiscard,
    /// Reconciliation: before an orphan history directory is removed. [lib/store.js:728]
    ReconcileOrphanHistory,
    /// Before a backfilled digest is written. [lib/store.js:766]
    BackfillWrite,
}

/// The verdict an armed failpoint produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectedFault {
    /// An in-process failure: the operation compensates exactly as Node's `catch` block does.
    Error(AppError),
    /// The process died at this point: no compensation runs, and the on-disk/database state is
    /// left exactly as of the boundary for startup reconciliation to repair.
    Crash(AppError),
}

impl InjectedFault {
    pub(super) fn into_error(self) -> AppError {
        match self {
            Self::Error(error) | Self::Crash(error) => error,
        }
    }
}

/// Test seam that turns any lifecycle boundary into a failure.
pub trait FaultInjector: Send + Sync + fmt::Debug {
    /// # Errors
    /// Returns the fault armed for `point`, if any.
    fn check(&self, point: FaultPoint) -> Result<(), InjectedFault>;
}

/// Production injector: never fails.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFaults;

impl FaultInjector for NoFaults {
    fn check(&self, _point: FaultPoint) -> Result<(), InjectedFault> {
        Ok(())
    }
}

#[derive(Debug)]
struct ArmedFault {
    point: FaultPoint,
    crash: bool,
    remaining: usize,
}

/// A deterministic injector: arm a point, it fires the configured number of times, and every
/// visited point is recorded so a test can prove the step was actually reached.
#[derive(Debug, Default)]
pub struct ScriptedFaults {
    armed: Mutex<Vec<ArmedFault>>,
    visited: Mutex<Vec<FaultPoint>>,
}

impl ScriptedFaults {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail once at `point` with a recoverable in-process error.
    #[must_use]
    pub fn fail_once(self, point: FaultPoint) -> Self {
        self.arm(point, false, 1)
    }

    /// Simulate a process death once at `point`: no compensation runs.
    #[must_use]
    pub fn crash_once(self, point: FaultPoint) -> Self {
        self.arm(point, true, 1)
    }

    fn arm(self, point: FaultPoint, crash: bool, remaining: usize) -> Self {
        if let Ok(mut armed) = self.armed.lock() {
            armed.push(ArmedFault {
                point,
                crash,
                remaining,
            });
        }
        self
    }

    /// Every failpoint the store reached, in order.
    #[must_use]
    pub fn visited(&self) -> Vec<FaultPoint> {
        self.visited
            .lock()
            .map(|visited| visited.clone())
            .unwrap_or_default()
    }

    /// True when every armed fault has fired.
    #[must_use]
    pub fn all_fired(&self) -> bool {
        self.armed
            .lock()
            .is_ok_and(|armed| armed.iter().all(|entry| entry.remaining == 0))
    }
}

impl FaultInjector for ScriptedFaults {
    fn check(&self, point: FaultPoint) -> Result<(), InjectedFault> {
        if let Ok(mut visited) = self.visited.lock() {
            visited.push(point);
        }
        let Ok(mut armed) = self.armed.lock() else {
            return Ok(());
        };
        for entry in armed.iter_mut() {
            if entry.point == point && entry.remaining > 0 {
                entry.remaining -= 1;
                let error = AppError::Unavailable(format!("injected fault at {point:?}"));
                return Err(if entry.crash {
                    InjectedFault::Crash(error)
                } else {
                    InjectedFault::Error(error)
                });
            }
        }
        Ok(())
    }
}

/// An aborted operation, carrying whether Node's `catch` block should run.
#[derive(Debug)]
pub(super) struct Interrupt {
    pub(super) error: AppError,
    pub(super) compensate: bool,
}

impl From<AppError> for Interrupt {
    fn from(error: AppError) -> Self {
        Self {
            error,
            compensate: true,
        }
    }
}

impl From<InjectedFault> for Interrupt {
    fn from(fault: InjectedFault) -> Self {
        let compensate = matches!(fault, InjectedFault::Error(_));
        Self {
            error: fault.into_error(),
            compensate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_faults_fire_once_and_record_visits() {
        let faults = ScriptedFaults::new().fail_once(FaultPoint::PublishRename);
        assert!(faults.check(FaultPoint::PublishInsert).is_ok());
        assert!(matches!(
            faults.check(FaultPoint::PublishRename),
            Err(InjectedFault::Error(_))
        ));
        assert!(faults.check(FaultPoint::PublishRename).is_ok());
        assert!(faults.all_fired());
        assert_eq!(
            faults.visited(),
            vec![
                FaultPoint::PublishInsert,
                FaultPoint::PublishRename,
                FaultPoint::PublishRename
            ]
        );
    }

    #[test]
    fn crash_faults_skip_compensation() {
        let faults = ScriptedFaults::new().crash_once(FaultPoint::PublishRename);
        let fault = faults
            .check(FaultPoint::PublishRename)
            .expect_err("armed fault fires");
        assert!(matches!(fault, InjectedFault::Crash(_)));
        assert!(!Interrupt::from(fault).compensate);
        assert!(NoFaults.check(FaultPoint::DeleteRow).is_ok());
    }
}

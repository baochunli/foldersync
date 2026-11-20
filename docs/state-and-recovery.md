# State and recovery

The app owns private local mapping and run records. Each mapping generation
keeps its own rclone work directory. Preserve that directory after an
interrupted run and keep recovery copies separate from live sync trees.

After a crash or disconnect, revalidate endpoints and volume identity before
continuing. A missing or damaged work directory, or a change to a mapping's
identity, requires an explicit recovery decision. Initialization and resync
must be deliberate actions.

## Implemented state dimensions (`sync-core`)

`sync_core::state::MappingState` models readiness, execution, and user intent
as three separate dimensions:

| Dimension | Implemented states |
| --- | --- |
| `Readiness` | `Unconfigured`, `NeedsInitialization`, `Ready`, `VolumeUnavailable`, `AuthenticationRequired`, `EndpointChanged`, `RecoveryRequired` |
| `Execution` | `Idle`, `Queued`, `Running`, `Stopping`, `Interrupted` |
| `Intent` | `Enabled`, `Paused` |

## User actions

| Action | Implemented transition |
| --- | --- |
| `CloseWindow` | No mapping state change; manager continues running. |
| `Pause` | Sets `Intent::Paused`; cancels a `Queued` run to `Idle` but lets a `Running` or `Stopping` run finish. |
| `Resume` | Sets `Intent::Enabled`; transitions `Idle` to `Queued` (`ScheduleRun`) only when `Readiness::Ready`. |
| `RunNow` | Transitions `Idle` to `Queued` (`ScheduleRun`) only when `Ready` and `Enabled`. |
| `StopRun` | Moves `Running` to `Stopping` (`RequestGracefulStop`) or `Queued` to `Idle`. |
| `ForceStop` | Moves `Running` or `Stopping` to `Execution::Interrupted` and `Readiness::RecoveryRequired` (`ForceTerminateChild`). |
| `QuitRequested` | Stops scheduling (`StopScheduling`); moves `Running` to `Stopping` without altering `Intent`. |

## Observed outcomes and recovery invariants

| Outcome (`RunOutcome`) | Implemented transition |
| --- | --- |
| `Success`, `SuccessWithConflicts`, `RetryableFailure` | `Execution::Idle`; preserves current `Readiness` and `Intent`. |
| `AuthenticationFailure` | `Execution::Idle`; sets `Readiness::AuthenticationRequired` (unless already `RecoveryRequired`). |
| `EndpointOrVolumeMismatch` | `Execution::Idle`; sets `Readiness::EndpointChanged` (unless already `RecoveryRequired` or `VolumeUnavailable`). |
| `InvalidConfiguration`, `CriticalFailure`, `UnknownFailure`, `CorruptState` | `Execution::Idle`; latches `Readiness::RecoveryRequired`. |
| `Interrupted` | `Execution::Interrupted`; latches `Readiness::RecoveryRequired`. |

Key enforced invariants:

- Reconnecting a volume (`ObservedEvent::VolumeAvailable`) never clears `Intent::Paused`.
- No transition clears `Intent::Paused` except `UserAction::Resume`.
- Once `Readiness::RecoveryRequired` is set, no user action or other event clears it except `ObservedEvent::RecoveryCompleted`.
- `status_text` reports explicit household status (`"Waiting for <volume>"`, `"Scanning <name>"`, `"Last completed at HH:MM"`, `"Completed with conflicts"`, `"Recovery required"`) and never claims `"Everything is up to date"`.

**TODO** verify cancellation and restart behavior, recovery copies, and resync
procedures using disposable fixtures.

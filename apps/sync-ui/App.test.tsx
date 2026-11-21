// Headless NSH tests for FolderSync UI (Group 3).
// Runs via `~/.local/bin/nsh test` in `apps/sync-ui`.
import { expect, open, test } from "nsh/test";

const DESKTOP = { width: 1120, height: 760, platform: "macos" as const };

test("Overview shows every state label, actions, Sync now verb, and Paper Mono path nodes", async () => {
  const app = await open(".", DESKTOP);
  const allTexts = app.texts();
  const joined = allTexts.join("\n");

  // Header + summary + Sync now verb everywhere (never "Run now")
  expect(allTexts).toContain("Overview");
  expect(allTexts).toContain("Sync ready mappings");
  expect(allTexts).toContain("Add mapping…");
  expect(allTexts).toContain("Sync now");
  expect(joined).not.toContain("Run now");

  // Volume groups with free space, low space, and missing drive
  expect(allTexts).toContain("Internal SSD");
  expect(allTexts).toContain("212 GB free of 994 GB");
  expect(allTexts).toContain("WorkSSD");
  expect(allTexts).toContain("1.4 TB free of 2 TB");
  expect(allTexts).toContain("PhotoSSD");
  expect(allTexts).toContain("Not connected");
  expect(allTexts).toContain("Free space unknown");
  expect(allTexts).toContain("ArchiveSSD");
  expect(allTexts).toContain("Low space: 260 GB free of 8 TB");

  // Every Readiness × Execution × Intent state label from spec §6
  expect(joined).toContain("Last completed at 14:38");
  expect(joined).toContain("Completed with conflicts at 14:32");
  expect(joined).toContain("Last run was interrupted at 09:14");
  expect(joined).toContain("Sign-in for lab has expired");
  expect(joined).toContain("Scanning Research · 3m 12s");
  expect(joined).toContain("Waiting for PhotoSSD");
  expect(joined).toContain("Cloud folder changed");
  expect(joined).toContain("Paused by you on 16 Nov");
  expect(joined).toContain("Setup not finished");
  expect(joined).toContain("First sync needed");
  expect(joined).toContain("Not run yet");
  expect(joined).toContain("Runs after Research");
  expect(joined).toContain("pauses after this run");
  expect(joined).toContain("Stopping · 0:24");
  expect(joined).toContain("Couldn't reach Google Drive · retrying at 14:50");

  // Actions enabled/disabled per §6
  expect(app.enabled("id:sync-now-documents")).toBe(true);
  expect(app.enabled("id:sync-now-photos-archive")).toBe(false);
  expect(app.enabled("id:sync-now-photos-family")).toBe(false);
  expect(app.enabled("id:sync-now-measurements")).toBe(false);
  expect(app.enabled("id:sync-now-teaching")).toBe(false);
  expect(app.shows("id:stop-research")).toBe(true);
  expect(app.shows("id:resume-photos-archive")).toBe(true);
  expect(app.shows("id:force-stop-media-stopping")).toBe(true);

  // Path nodes exist and use the Paper Mono style (11 pt)
  expect(app.shows("id:path-documents")).toBe(true);
  expect(app.shows("id:path-research")).toBe(true);
  expect(app.spanStyle("text:household:Data/Documents").fontSize).toBe(11);
  expect(app.spanStyle("text:household:Data/Research").fontSize).toBe(11);
});

test("Attention count badge is 5 and marking conflicts reviewed updates it to 4", async () => {
  const app = await open(".", DESKTOP);

  expect(app.shows("id:sidebar-attention-count")).toBe(true);
  expect(app.texts()).toContain("5");

  await app.press("id:nav-attention");
  const joined = app.texts().join("\n");
  expect(joined).toContain("5 items · items that block syncing first");
  expect(joined).toContain("Cause:");
  expect(joined).toContain("What FolderSync did:");
  expect(joined).toContain("Next:");
  expect(joined).toContain("Family · completed with conflicts");

  // Mark conflicts reviewed -> count drops from 5 to 4
  await app.press("id:mark-reviewed-family");
  expect(app.texts().join("\n")).toContain("4 items · items that block syncing first");
});

test("Add-mapping review blocks both-populated with no Start first sync button", async () => {
  const app = await open(".", DESKTOP);

  await app.press("id:add-mapping-btn");
  // Step 1 is skipped automatically when only one connected account is usable (§5.4)
  expect(app.texts()).toContain("Choose a folder");
  expect(app.texts()).toContain("Where Projects fits");

  // Step 1 is still accessible and shows connected vs held accounts
  await app.press("id:wizard-step-1");
  expect(app.texts()).toContain("Add mapping");
  expect(app.texts()).toContain("Which Google account holds the folder?");

  // Jump to step 6 (Review)
  await app.press("id:wizard-step-6");
  expect(app.texts()).toContain("Review “Projects”");
  expect(app.shows("id:wizard-start-first-sync-btn")).toBe(true);
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(false);

  // Switch to Both populated (blocked) variant
  await app.press("id:review-variant-both-populated");
  const joined = app.texts().join("\n");
  expect(joined).toContain("Can't start");
  expect(joined).toContain("Both sides already contain files");
  expect(joined).toContain("Choose another folder…");
  expect(app.shows("id:wizard-start-first-sync-btn")).toBe(false);
});

test("Interactive Pause/Resume, Scheduling hold banner, and window close keep-alive work", async () => {
  const app = await open(".", DESKTOP);

  // Pause Documents -> Resume appears and Sync now becomes disabled
  await app.press("id:pause-documents");
  expect(app.shows("id:resume-documents")).toBe(true);
  expect(app.enabled("id:sync-now-documents")).toBe(false);

  // Resume Documents -> Sync now re-enabled (or queued)
  await app.press("id:resume-documents");
  expect(app.shows("id:pause-documents")).toBe(true);

  // Pause scheduling -> global hold banner appears, Sync now still allowed (D1)
  await app.press("id:demo-toggle-scheduling");
  expect(app.shows("id:scheduling-paused-banner")).toBe(true);
  expect(app.texts().join("\n")).toContain("Scheduling paused. No mapping starts on its own. Sync now still works.");

  // Closing the window hides it (keep-alive-on-close) and Show window reopens it (§3, #11)
  expect(app.window("main").state()?.visible).toBe(true);
  await app.window("main").pressClose();
  expect(app.window("main").state()?.visible).toBe(false);
  await app.keyboard.press("mod+0");
  expect(app.window("main").state()?.visible).toBe(true);
  expect(app.texts()).toContain("Overview");
});

test("Mapping detail hides Change subfolders and Activity raw log toggles open", async () => {
  const app = await open(".", DESKTOP);

  // Open Family detail
  await app.press("id:open-detail-family");
  const detailText = app.texts().join("\n");
  expect(detailText).toContain("Included subfolders");
  expect(detailText).not.toContain("Change subfolders");
  expect(detailText).toContain("Sync now");

  // Activity screen: indeterminate scanning + elapsed time, raw log closed by default
  await app.press("id:nav-activity");
  const actText = app.texts().join("\n");
  expect(actText).toContain("Scanning Research");
  expect(actText).toContain("Nothing yet");
  expect(app.shows("id:activity-raw-log-body")).toBe(false);

  await app.press("id:activity-raw-log-toggle");
  expect(app.shows("id:activity-raw-log-body")).toBe(true);
});

test("Preferences Diagnostics shows engine flags only there and Welcome states render", async () => {
  const app = await open(".", DESKTOP);

  // Overview must never show raw rclone flags
  expect(app.texts().join("\n")).not.toContain("--max-delete");

  // Preferences -> General vs Diagnostics
  await app.press("id:nav-preferences");
  expect(app.texts().join("\n")).toContain("Open at login");
  expect(app.texts().join("\n")).not.toContain("--max-delete");

  await app.press("id:prefs-tab-diagnostics");
  const diagText = app.texts().join("\n");
  expect(diagText).toContain("--max-delete 10");
  expect(diagText).toContain("Export diagnostics…");

  // Welcome empty state
  await app.press("id:demo-welcome-empty");
  expect(app.texts()).toContain("Keep the Drive folders you choose on this Mac");
  expect(app.texts()).toContain("Connect Google Drive…");
});

test("Wizard gates Start and step pills on validation checks and resets consent on plan changes (#2)", async () => {
  const app = await open(".", DESKTOP);

  await app.press("id:add-mapping-btn");
  // Move to Step 3 (Location)
  await app.press("id:wizard-step-3");
  expect(app.texts()).toContain("Where should Projects live?");

  // Trigger invalid volume-root location -> Continue disabled and Review step pill is disabled (cannot skip past Location)
  await app.press("id:wizard-demo-invalid-root");
  expect(app.enabled("id:wizard-continue-btn")).toBe(false);
  expect(app.shows("id:wizard-step-6")).toBe(true);
  expect(app.enabled("id:wizard-step-6")).toBe(false);
  expect(app.texts()).toContain("Where should Projects live?");
  expect(app.texts()).not.toContain("Review “Projects”");

  // Restore valid folder -> jump to Review, confirm checkbox -> Start enabled
  await app.press("id:wizard-demo-valid-folder");
  expect(app.enabled("id:wizard-continue-btn")).toBe(true);
  await app.press("id:wizard-step-6");
  expect(app.texts()).toContain("Review “Projects”");
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(false);

  await app.press("id:wizard-no-other-sync-checkbox");
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(true);

  // Changing the reviewed plan (switching scenario and back) resets consent -> Start disabled again
  await app.press("id:review-variant-upload");
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(false);
});

test("Activity Stop targets the displayed mapping, ⌘R targets only selected MappingDetail, and Detail supports Force stop and Recovery link (#5, #13)", async () => {
  const app = await open(".", DESKTOP);

  // ⌘R outside MappingDetail (e.g. Accounts) must do nothing (menu item disabled)
  await app.press("id:nav-accounts");
  await app.keyboard.press("mod+r");
  await app.press("id:nav-overview");
  // Documents is still idle (not queued)
  expect(app.texts().join("\n")).toContain("Last completed at 14:38");

  // ⌘R inside MappingDetail for Documents targets Documents only (queues it behind Research)
  await app.press("id:open-detail-documents");
  await app.keyboard.press("mod+r");
  expect(app.texts().join("\n")).toContain("Runs after Research");

  // Activity preview keeps the displayed mapping consistent with its Stop target (Research)
  await app.press("id:nav-activity");
  await app.press("id:activity-phase-known");
  expect(app.texts()).toContain("Research");
  expect(app.texts().join("\n")).toContain("Transferring · 1.2 GB of 3.4 GB");

  // Stopping from Activity stops Research (the displayed mapping), transitioning to Stopping + Force stop…
  await app.press("id:activity-stop-btn");
  expect(app.texts().join("\n")).toContain("Stopping ·");
  expect(app.texts()).toContain("Force stop…");

  // Detail for Research (now Stopping) shows Force stop… too (#13)
  await app.press("id:nav-overview");
  await app.press("id:open-detail-research");
  expect(app.shows("id:detail-force-stop-btn")).toBe(true);

  // Detail for Archive shows cloud folder ID changed since setup (not hardcoded "matches setup") (#8)
  await app.press("id:nav-overview");
  await app.press("id:open-detail-archive");
  expect(app.texts().join("\n")).toContain("changed since setup");

  // Detail for Teaching routes "Start recovery…" to the Recovery checklist (#13)
  await app.press("id:nav-overview");
  await app.press("id:open-detail-teaching");
  await app.press("text:Start recovery…");
  expect(app.texts()).toContain("Recover “Teaching”");
});

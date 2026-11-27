// Headless NSH tests for FolderSync UI (Group 3).
// Runs via `~/.local/bin/nsh test` in `apps/sync-ui`.
import { expect, open, test } from "nsh/test";

const DESKTOP = { width: 1120, height: 760, platform: "macos" as const };

async function startDemoSignIn(app: Awaited<ReturnType<typeof open>>) {
  await app.press("id:accounts-connect-btn");
  await app.type("id:accounts-account-id", "work-2");
  await app.press("id:accounts-submit-btn");
}

async function pickDemoFolder(app: Awaited<ReturnType<typeof open>>) {
  await app.press("id:wizard-step-3");
  await app.press("id:wizard-demo-valid-folder");
}

test("Overview shows every state label, actions, Sync now verb, and Paper Mono path nodes", async () => {
  const app = await open(".", DESKTOP);
  const allTexts = app.texts();
  const joined = allTexts.join("\n");

  // Header + summary + Sync now verb everywhere (never "Run now")
  expect(allTexts).toContain("Overview");
  expect(allTexts).toContain("Sync all now");
  expect(allTexts).toContain("Add mapping…");
  expect(allTexts).toContain("Sync now");
  expect(joined).not.toContain("Run now");

  // Volume groups with free space, low space, and missing drive
  expect(allTexts).toContain("Internal SSD");
  expect(allTexts).toContain("212 GB free of 994 GB");
  expect(allTexts).toContain("APFS");
  expect(allTexts).toContain("WorkSSD");
  expect(allTexts).toContain("1.4 TB free of 2 TB");
  expect(allTexts).toContain("PhotoSSD");
  expect(allTexts).toContain("Not connected");
  expect(allTexts).toContain("Free space unknown");
  expect(allTexts).toContain("ArchiveSSD");
  expect(allTexts).toContain("Low space: 26 GB free of 8 TB");

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

test("Overview uses compact mockup typography and keeps names actionable", async () => {
  const app = await open(".", DESKTOP);

  const name = app.spanStyle("id:open-detail-documents >> text:Documents");
  expect(name.fontSize).toBe(13.5);
  expect(name.fontWeight).toBe(500);
  expect(name.color).toBe("#1c1c1eff");
  expect(app.spanStyle("id:nav-activity >> text:Activity").fontWeight).toBe(300);
  expect(app.spanStyle("id:nav-overview >> text:Overview").fontWeight).toBe(500);
  expect(app.shows("id:open-detail-documents")).toBe(true);
  await app.press("id:open-detail-documents");
  expect(app.texts()).toContain("Included subfolders");
  await app.press("id:nav-activity");
  expect(app.spanStyle("id:nav-activity >> text:Activity").fontWeight).toBe(500);
  expect(app.spanStyle("id:nav-overview >> text:Overview").fontWeight).toBe(300);
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
  expect(joined).toContain("Held the mapping while the disk is disconnected.");
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
  await pickDemoFolder(app);
  await app.press("id:wizard-step-6");
  expect(app.texts()).toContain("Review “Projects”");
  expect(app.shows("id:wizard-start-first-sync-btn")).toBe(true);
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(false);

  expect(app.texts()).not.toContain("Automatic sync");
  expect(app.texts().join("\n")).not.toContain("Starts only after");

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
  await app.press("id:toggle-scheduling");
  expect(app.shows("id:scheduling-paused-banner")).toBe(true);
  expect(app.texts().join("\n")).toContain("Scheduling paused. No mapping starts on its own. Sync now still works.");
  expect(app.texts().join("\n")).not.toContain("retrying at 14:50");

  // Closing the window hides it (keep-alive-on-close) and Show window reopens it (§3, #11)
  expect(app.window("main").state()?.visible).toBe(true);
  await app.window("main").pressClose();
  expect(app.window("main").state()?.visible).toBe(false);
  await app.keyboard.press("mod+0");
  expect(app.window("main").state()?.visible).toBe(true);
  expect(app.texts()).toContain("Overview");
});

test("Mapping schedule, volume, and subfolder change review; Activity raw log toggles open", async () => {
  const app = await open(".", DESKTOP);

  // Open Family detail
  await app.press("id:open-detail-family");
  const detailText = app.texts().join("\n");
  expect(detailText).toContain("Included subfolders");
  expect(detailText).toContain("Change subfolders…");
  expect(detailText).toContain("Automatic sync on");
  expect(detailText).toContain("212 GB free of 994 GB");

  await app.press("id:change-subfolders-btn");
  expect(app.texts().join("\n")).toContain("Free on Internal SSD");
  await app.press("id:subfolder-photos");
  await app.press("id:filter-change-preview");
  expect(app.texts().join("\n")).toContain("Previous sync setup is kept");
  expect(app.enabled("id:filter-change-confirm")).toBe(false);
  expect(app.texts().join("\n")).toContain("/Photos/**");
  await app.press("id:filter-change-consent");
  expect(app.enabled("id:filter-change-confirm")).toBe(true);
  await app.press("id:filter-change-confirm");
  expect(app.texts().join("\n")).toContain("First sync");
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

test("Custom exclusions can be removed individually from a new mapping", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:add-mapping-btn");
  await pickDemoFolder(app);
  await app.press("id:wizard-step-4");

  await app.type("id:wizard-pattern-input", ".git/");
  await app.press("id:wizard-add-pattern-btn");
  await app.type("id:wizard-pattern-input", "*.bak");
  await app.press("id:wizard-add-pattern-btn");
  expect(app.texts()).toContain(".git/");
  await app.press("id:remove-pattern-.git/");
  expect(app.texts()).not.toContain(".git/");
  expect(app.shows("id:remove-pattern-*.bak")).toBe(true);

  await app.type("id:wizard-pattern-input", ".DS_Store");
  await app.press("id:wizard-add-pattern-btn");
  await app.press("id:wizard-default-exclusions-toggle");
  expect(app.texts()).toContain(".DS_Store");
  expect(app.shows("id:remove-pattern-.DS_Store")).toBe(false);
});

test("Removing an existing custom exclusion requires filter review and confirmation", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:open-detail-family");
  await app.press("id:change-subfolders-btn");
  await app.press("id:remove-pattern-*.lrdata/");
  expect(app.texts()).not.toContain("*.lrdata/");
  await app.press("id:filter-change-preview");
  expect(app.texts().join("\n")).toContain("Removed exclusions\n*.lrdata/");
  expect(app.enabled("id:filter-change-confirm")).toBe(false);
  await app.press("id:filter-change-consent");
  expect(app.enabled("id:filter-change-confirm")).toBe(true);
  await app.press("id:filter-change-confirm");
  expect(app.texts().join("\n")).toContain("First sync");
  await app.press("id:nav-overview");
  await app.press("id:open-detail-family");
  expect(app.texts().join("\n")).not.toContain("*.lrdata/");
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

  // Detail for Archive shows setup ID and found ID on separate explicitly labeled rows (#8)
  await app.press("id:nav-overview");
  await app.press("id:open-detail-archive");
  const archiveText = app.texts().join("\n");
  expect(archiveText).toContain("At setup");
  expect(archiveText).toContain("1Ar…k2P");
  expect(archiveText).toContain("Found now");
  expect(archiveText).toContain("1Zx…m9T");
  expect(archiveText).toContain("changed since setup");

  // Detail for Teaching routes "Start recovery…" to the inspected Recovery checklist (#9, #13)
  await app.press("id:nav-overview");
  await app.press("id:open-detail-teaching");
  await app.press("text:Start recovery…");
  const recText = app.texts().join("\n");
  expect(recText).toContain("Recover “Teaching”");
  expect(recText).toContain("No earlier sync process is running.");
  expect(recText).toContain("Local and cloud folders match setup.");
  expect(recText).toContain("Last run was interrupted at 09:14");
  expect(recText).not.toContain("log ends during uploads");
});

test("Both populated block recovers when choosing a valid replacement folder and reaches Review with fresh consent (round 2 #1)", async () => {
  const app = await open(".", DESKTOP);

  await app.press("id:add-mapping-btn");
  await pickDemoFolder(app);
  await app.press("id:wizard-step-6");
  expect(app.texts()).toContain("Review “Projects”");

  // Trigger BothPopulated block -> no Start button
  await app.press("id:review-variant-both-populated");
  expect(app.texts()).toContain("Both sides already contain files");
  expect(app.shows("id:wizard-start-first-sync-btn")).toBe(false);

  // Choose another folder… -> Location still blocked until destination changes
  await app.press("text:Choose another folder…");
  expect(app.texts()).toContain("Where should Projects live?");
  expect(app.enabled("id:wizard-continue-btn")).toBe(false);
  expect(app.enabled("id:wizard-step-6")).toBe(false);

  // Pick a valid replacement folder -> re-derives scenario, enables Continue & Review pill
  await app.press("id:wizard-demo-valid-folder");
  expect(app.texts()).toContain("/Volumes/WorkSSD/Projects-New");
  expect(app.enabled("id:wizard-continue-btn")).toBe(true);
  expect(app.enabled("id:wizard-step-6")).toBe(true);

  // Reach Review with fresh (unchecked) consent
  await app.press("id:wizard-step-6");
  expect(app.texts()).toContain("Review “Projects”");
  expect(app.texts()).toContain("First sync: download from the cloud");
  expect(app.shows("id:wizard-start-first-sync-btn")).toBe(true);
  expect(app.enabled("id:wizard-start-first-sync-btn")).toBe(false);
});

test("Deferred UI fixes: resumable draft, new folder, queue order, conflict preservation, run inspection, and accounts (#3,#4,#6,#7,#9,#10)", async () => {
  const app = await open(".", DESKTOP);

  // #7: Pausing Family must preserve its unreviewed conflicts; Mark as reviewed clears badge while keeping history
  await app.press("id:pause-family");
  expect(app.texts()).toContain("5");
  await app.press("id:nav-attention");
  expect(app.texts().join("\n")).toContain("Family · completed with conflicts");

  // #9: Attention "Show recovery copies" opens run-specific log & recovery copies view
  await app.press("id:attn-show-recovery-copies-teaching");
  const inspText = app.texts().join("\n");
  expect(inspText).toContain("Run details · Teaching");
  expect(inspText).toContain("Recovery copies from this run");
  expect(inspText).toContain("Lectures/week-04-slides.pdf");
  expect(inspText).toContain("Copy to inspection folder…");

  // #6: Queue order shows queued mappings in order and Sync now moves queued mapping to front
  await app.press("id:nav-overview");
  await app.press("id:sync-ready-btn");
  await app.press("id:nav-activity");
  expect(app.texts().join("\n")).toContain("Queued (in order):");

  // #10: Cancel reconnect on Connected account restores Connected; Connect account adds a new account
  await app.press("id:nav-accounts");
  await startDemoSignIn(app);
  await app.press("id:accounts-simulate-signin-btn");
  expect(app.texts().join("\n")).toContain("work-2");

  // #3 & #4: With 2 connected accounts, Step 1 (Account) is shown; advance to Folder, create a new empty folder, and complete upload first sync
  await app.press("id:nav-overview");
  await app.press("id:add-mapping-btn");
  expect(app.texts()).toContain("Which Google account holds the folder?");
  await app.press("id:wizard-continue-btn");
  await app.press("id:wizard-new-folder-btn");
  await app.type("id:wizard-new-folder-name", "New-Folder");
  await app.press("id:wizard-create-folder-btn");
  expect(app.texts().join("\n")).toContain("New-Folder");
  await pickDemoFolder(app);
  await app.press("id:wizard-step-6");
  expect(app.texts().join("\n")).toContain("First sync: upload to the cloud");
  expect(app.texts().join("\n")).toContain("creates a new access marker file");
  await app.press("id:wizard-no-other-sync-checkbox");
  await app.press("id:wizard-start-first-sync-btn");
  expect(app.texts().join("\n")).toContain("Uploading to the cloud");
  await app.press("id:wizard-demo-complete-first-sync-btn");
  expect(app.texts().join("\n")).toContain("Automatic sync is now on");
});


test("Accounts show sign-in, browser wait, and connected states", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:nav-accounts");
  expect(app.texts().join("\n")).toContain("Sign-in required");
  await startDemoSignIn(app);
  expect(app.texts().join("\n")).toContain("Connect Google Drive");
  expect(app.texts().join("\n")).toContain("Finish signing in in your browser");
  await app.press("id:accounts-simulate-signin-btn");
  expect(app.texts().join("\n")).toContain("work-2");
});

test("Connect Google Drive explains and validates Account ID before sign-in", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:nav-accounts");
  await app.press("id:accounts-connect-btn");
  expect(app.texts().join("\n")).toContain("Choose a name for this Mac (e.g. personal)");

  const submit = "id:accounts-submit-btn";
  const field = "id:accounts-account-id";
  expect(app.enabled(submit)).toBe(false);
  for (const invalid of ["-personal", "_personal", "my account", "a/b", "équipe", "a".repeat(129)]) {
    await app.type(field, invalid);
    expect(app.enabled(submit)).toBe(false);
    await app.press("id:accounts-cancel-btn");
    await app.press("id:accounts-connect-btn");
  }
  await app.type(field, "a".repeat(128));
  expect(app.enabled(submit)).toBe(true);
  await app.press("id:accounts-cancel-btn");
  await app.press("id:accounts-connect-btn");
  await app.type(field, "1-work_2");
  expect(app.enabled(submit)).toBe(true);
  await app.press(submit);
  await app.press("id:accounts-simulate-signin-btn");
  expect(app.texts().join("\n")).toContain("1-work_2");
});

test("Location shows guidance, disk-check progress, and completed results", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:add-mapping-btn");
  await app.press("id:wizard-step-3");

  const next = "id:wizard-continue-btn";
  expect(app.texts()).toContain("Choose a folder to see its disk and free space.");
  expect(app.shows("id:wizard-location-capacity")).toBe(false);
  expect(app.shows("id:wizard-location-checks")).toBe(false);
  expect(app.enabled(next)).toBe(false);

  await app.press("id:wizard-demo-checking-folder");
  expect(app.texts()).toContain("/Volumes/WorkSSD/Projects");
  expect(app.texts()).toContain("Checking disk and free space…");
  expect(app.shows("id:wizard-location-capacity")).toBe(false);
  expect(app.shows("id:wizard-location-checks")).toBe(false);
  expect(app.enabled(next)).toBe(false);
  expect(app.enabled("id:wizard-step-4")).toBe(false);

  await app.press("id:wizard-demo-invalid-root");
  expect(app.texts().join("\n")).toContain("top of the volume");
  expect(app.texts()).not.toContain("Checking disk and free space…");
  expect(app.enabled(next)).toBe(false);

  await app.press("id:wizard-demo-valid-folder");
  expect(app.shows("id:wizard-location-capacity")).toBe(true);
  expect(app.enabled(next)).toBe(true);
});

test("Folder choices show on-demand size, cancellation, and measurement failure", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:add-mapping-btn");
  expect(app.texts()).toContain("Projects");
  expect(app.texts()).toContain("Old Projects · Duplicate name");
  expect(app.texts()).toContain("Draft:Notes · Unsupported name");
  expect(app.texts().join("\n")).toContain("18.6 GB · 2410 files");
  await app.press("id:wizard-folder-Shared-Media");
  expect(app.texts()).toContain("Calculating…");
  await app.press("id:wizard-cancel-size-btn");
  expect(app.texts()).not.toContain("Calculating…");
  await app.press("id:wizard-folder-Shared-Media");
  await app.press("id:wizard-demo-fail-size-btn");
  expect(app.texts()).toContain("Some files did not report a size.");
});

test("New folder refuses a duplicate name in Data", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:add-mapping-btn");
  await app.press("id:wizard-new-folder-btn");
  await app.type("id:wizard-new-folder-name", "Discard this name");
  await app.press("id:wizard-cancel-new-folder-btn");
  await app.press("id:wizard-new-folder-btn");
  expect(app.enabled("id:wizard-create-folder-btn")).toBe(false);
  await app.type("id:wizard-new-folder-name", "Projects");
  await app.press("id:wizard-create-folder-btn");
  expect(app.texts().join("\n")).toContain("The name `Projects` is already used in Data.");
  await app.press("id:wizard-new-folder-btn");
  expect(app.enabled("id:wizard-create-folder-btn")).toBe(false);
});

test("Cancel sign-in clears the browser waiting card", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:nav-accounts");
  await startDemoSignIn(app);
  await app.press("id:accounts-cancel-reconnect-btn");
  expect(app.texts()).not.toContain("Finish signing in in your browser");
  expect(app.texts()).not.toContain("work-2");
});

test("Low-space advisory names the synced mappings and omits off-volume recovery", async () => {
  const app = await open(".", DESKTOP);
  await app.press("id:nav-attention");
  expect(app.texts()).toContain("Cause: Archive and Photos-Archive sync to ArchiveSSD.");
  expect(app.texts()).not.toContain("Reveal recovery folder");
});

test("Overview and Detail show the next check and distinguish the wrong disk", async () => {
  const app = await open(".", DESKTOP);
  expect(app.texts()).toContain("Next check in about 7 minutes");
  expect(app.texts()).toContain("A different disk named PhotoSSD is connected");
  await app.press("id:open-detail-documents");
  expect(app.texts()).toContain("In about 7 minutes");
  await app.press("id:nav-overview");
  await app.press("id:open-detail-photos-wrong-disk");
  expect(app.texts()).toContain("A different disk named PhotoSSD is connected");
});

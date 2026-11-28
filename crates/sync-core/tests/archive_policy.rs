use sync_core::archive::archive_suggestion;

#[test]
fn suggestion_requires_one_hundred_files_and_twelve_calendar_months() {
    // March 1, 2024 to March 1, 2025 spans 365 days.
    let now = 1_740_787_200;
    let cutoff = 1_709_251_200;
    assert!(archive_suggestion("Done".into(), 99, 16, cutoff, now).is_none());
    assert!(archive_suggestion("Done".into(), 100, 16, cutoff + 1, now).is_none());

    let result = archive_suggestion("Done".into(), 100, 16, cutoff, now).unwrap();
    assert_eq!(result.name, "Done");
    assert_eq!(result.file_count, 100);
    assert_eq!(result.bytes, 16);
    assert_eq!(result.last_changed_unix_secs, cutoff);
    assert_eq!(result.estimated_seconds_saved, 40);

    // March 1, 2023 to March 1, 2024 spans 366 days.
    let now = 1_709_251_200;
    let cutoff = 1_677_628_800;
    assert!(archive_suggestion("Done".into(), 100, 16, cutoff + 1, now).is_none());
    assert!(archive_suggestion("Done".into(), 100, 16, cutoff, now).is_some());
}

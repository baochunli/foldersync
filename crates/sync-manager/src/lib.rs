//! Coordinates scheduling, durable run records, volume checks, initialization,
//! and recovery for FolderSync.

#[cfg(test)]
mod tests {
    #[test]
    fn package_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "sync-manager");
    }
}

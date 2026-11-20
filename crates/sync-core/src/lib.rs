//! Owns FolderSync's mapping types, policies, readiness and execution states,
//! and the validation rules that protect mappings and run requests.

#[cfg(test)]
mod tests {
    #[test]
    fn package_name() {
        assert_eq!(env!("CARGO_PKG_NAME"), "sync-core");
    }
}

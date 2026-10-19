//! The rules for hosted mode (see `web/host.rs`) that don't need a browser, so they are tested
//! natively.

/// Longest file name kept from the host.
pub const MAX_NAME_CHARS: usize = 255;
/// The commands a host may run. Saving is how it collects the document; undo and redo let its own
/// shortcuts reach the document.
pub const HOST_COMMANDS: &[&str] = &["file.save", "edit.undo", "edit.redo"];

/// The last part of a host-given name, trimmed to a sane length; `None` for an empty one.
pub fn clean_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if last.is_empty() {
        return None;
    }
    Some(last.chars().take(MAX_NAME_CHARS).collect())
}

/// Whether a host may run this command.
pub fn host_may_run(id: &str) -> bool {
    HOST_COMMANDS.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_their_last_part_and_a_sane_length() {
        assert_eq!(clean_name(r"C:\notes\Report.docx").as_deref(), Some("Report.docx"));
        assert_eq!(clean_name("a/b/Plan.odt").as_deref(), Some("Plan.odt"));
        assert_eq!(clean_name("  spaced.docx ").as_deref(), Some("spaced.docx"));
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name("folder/"), None);
        assert_eq!(clean_name(&"é".repeat(400)).map(|n| n.chars().count()), Some(MAX_NAME_CHARS));
    }

    #[test]
    fn a_host_may_only_save_undo_and_redo() {
        assert!(host_may_run("file.save"));
        assert!(host_may_run("edit.undo"));
        assert!(!host_may_run("file.close"));
        assert!(!host_may_run(""));
    }
}

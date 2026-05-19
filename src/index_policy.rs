use crate::row_identity::SourceFingerprint;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RefreshMode {
    FullWindow,
    LocalIdCursor,
}

pub(crate) fn source_refresh_mode(
    previous: Option<SourceFingerprint>,
    current: SourceFingerprint,
) -> Option<RefreshMode> {
    let previous = match previous {
        Some(previous) if previous == current => return None,
        Some(previous) => previous,
        None => return Some(RefreshMode::FullWindow),
    };

    if current.shrank_from(previous) {
        Some(RefreshMode::FullWindow)
    } else {
        Some(RefreshMode::LocalIdCursor)
    }
}

pub(crate) fn overlap_since(
    indexed_latest: Option<i64>,
    minimum_since: i64,
    overlap_secs: i64,
) -> i64 {
    indexed_latest
        .map(|latest| (latest - overlap_secs).max(minimum_since))
        .unwrap_or(minimum_since)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(mtime_ns: i64, size: i64) -> SourceFingerprint {
        SourceFingerprint { mtime_ns, size }
    }

    #[test]
    fn unchanged_source_produces_no_refresh() {
        assert_eq!(source_refresh_mode(Some(fp(10, 100)), fp(10, 100)), None);
    }

    #[test]
    fn missing_previous_source_uses_full_window() {
        assert_eq!(
            source_refresh_mode(None, fp(10, 100)),
            Some(RefreshMode::FullWindow)
        );
    }

    #[test]
    fn shrinking_source_uses_full_window() {
        assert_eq!(
            source_refresh_mode(Some(fp(10, 100)), fp(11, 90)),
            Some(RefreshMode::FullWindow)
        );
    }

    #[test]
    fn growing_source_uses_local_id_cursor() {
        assert_eq!(
            source_refresh_mode(Some(fp(10, 100)), fp(11, 120)),
            Some(RefreshMode::LocalIdCursor)
        );
    }

    #[test]
    fn changed_mtime_with_same_size_uses_local_id_cursor() {
        assert_eq!(
            source_refresh_mode(Some(fp(10, 100)), fp(11, 100)),
            Some(RefreshMode::LocalIdCursor)
        );
    }

    #[test]
    fn overlap_since_never_goes_before_minimum() {
        assert_eq!(overlap_since(Some(100), 50, 75), 50);
        assert_eq!(overlap_since(Some(200), 50, 75), 125);
        assert_eq!(overlap_since(None, 50, 75), 50);
    }
}

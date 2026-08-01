use rusqlite::{ffi, Connection};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

const TOKENIZER_NAME: &[u8] = b"MMFtsTokenizer\0";
const BASE_TOKENIZER_NAME: &[u8] = b"unicode61\0";
const FTS5_API_POINTER_TYPE: &[u8] = b"fts5_api_ptr\0";
const FTS5_API_QUERY: &[u8] = b"SELECT fts5(?1)\0";

unsafe extern "C" {
    /// # Safety
    /// `stmt` must be a live SQLite statement and `pointer_type` must be a
    /// NUL-terminated type name accepted by SQLite.
    fn sqlite3_bind_pointer(
        stmt: *mut ffi::sqlite3_stmt,
        index: c_int,
        value: *mut c_void,
        pointer_type: *const c_char,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
}

struct TokenizerAlias {
    base_context: *mut c_void,
    create: unsafe extern "C" fn(
        *mut c_void,
        *mut *const c_char,
        c_int,
        *mut *mut ffi::Fts5Tokenizer,
    ) -> c_int,
    delete: unsafe extern "C" fn(*mut ffi::Fts5Tokenizer),
    tokenize: TokenizeFn,
}

struct TokenizerInstance {
    base: *mut ffi::Fts5Tokenizer,
    delete: unsafe extern "C" fn(*mut ffi::Fts5Tokenizer),
    tokenize: TokenizeFn,
}

type TokenCallback =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int;

type TokenizeFn = unsafe extern "C" fn(
    *mut ffi::Fts5Tokenizer,
    *mut c_void,
    c_int,
    *const c_char,
    c_int,
    Option<TokenCallback>,
) -> c_int;

struct OffsetCallback {
    context: *mut c_void,
    callback: TokenCallback,
    offset: c_int,
}

pub(crate) fn register_compatible_tokenizer(conn: &Connection) -> Result<(), String> {
    // SAFETY: `conn` owns a live SQLite handle for the duration of this call.
    // The registered callbacks and their boxed context are retained by SQLite
    // until the connection invokes `drop_alias`.
    unsafe { register_compatible_tokenizer_raw(conn.handle()) }
}

pub(crate) fn ideograph_match_query(query: &str) -> Option<&str> {
    let query = query.trim();
    (!query.is_empty() && query.chars().all(is_ideograph)).then_some(query)
}

/// # Safety
/// `db` must be a live SQLite connection handle that is not concurrently used
/// while the tokenizer is registered.
unsafe fn register_compatible_tokenizer_raw(db: *mut ffi::sqlite3) -> Result<(), String> {
    // SAFETY: `db` satisfies this function's contract and remains live.
    let api = unsafe { fts5_api(db)? };
    // SAFETY: `api` was returned by SQLite and checked for null by `fts5_api`.
    let find = unsafe { (*api).xFindTokenizer }
        .ok_or_else(|| "SQLite FTS5 tokenizer lookup is unavailable".to_string())?;
    // SAFETY: `api` was returned by SQLite and checked for null by `fts5_api`.
    let create = unsafe { (*api).xCreateTokenizer }
        .ok_or_else(|| "SQLite FTS5 tokenizer registration is unavailable".to_string())?;

    let mut existing_context = ptr::null_mut();
    let mut existing = empty_tokenizer();
    // SAFETY: `api` and its lookup callback are live; both output pointers
    // refer to initialized writable storage.
    let rc = unsafe {
        find(
            api,
            TOKENIZER_NAME.as_ptr().cast(),
            &mut existing_context,
            &mut existing,
        )
    };
    if rc == ffi::SQLITE_OK {
        return Ok(());
    }

    let mut base_context = ptr::null_mut();
    let mut base = empty_tokenizer();
    // SAFETY: `api` and its lookup callback are live; both output pointers
    // refer to initialized writable storage.
    let rc = unsafe {
        find(
            api,
            BASE_TOKENIZER_NAME.as_ptr().cast(),
            &mut base_context,
            &mut base,
        )
    };
    if rc != ffi::SQLITE_OK {
        // SAFETY: `db` is the live handle required by this function's contract.
        return Err(unsafe { sqlite_error(db, "Find SQLite unicode tokenizer", rc) });
    }
    let base_create = base
        .xCreate
        .ok_or_else(|| "SQLite unicode tokenizer has no constructor".to_string())?;
    let base_delete = base
        .xDelete
        .ok_or_else(|| "SQLite unicode tokenizer has no destructor".to_string())?;
    let base_tokenize = base
        .xTokenize
        .ok_or_else(|| "SQLite unicode tokenizer cannot tokenize".to_string())?;
    let alias = Box::new(TokenizerAlias {
        base_context,
        create: base_create,
        delete: base_delete,
        tokenize: base_tokenize,
    });
    let alias_ptr = Box::into_raw(alias).cast::<c_void>();
    let mut tokenizer = ffi::fts5_tokenizer {
        xCreate: Some(create_alias),
        xDelete: Some(delete_alias),
        xTokenize: Some(tokenize_alias),
    };
    // SAFETY: `api` is live, `alias_ptr` owns the registered context, and the
    // tokenizer callbacks use the ABI required by FTS5.
    let rc = unsafe {
        create(
            api,
            TOKENIZER_NAME.as_ptr().cast(),
            alias_ptr,
            &mut tokenizer,
            Some(drop_alias),
        )
    };
    if rc != ffi::SQLITE_OK {
        // SAFETY: SQLite did not take ownership after failed registration, so
        // this is still the exact pointer returned by `Box::into_raw` above.
        unsafe { drop(Box::from_raw(alias_ptr.cast::<TokenizerAlias>())) };
        // SAFETY: `db` is the live handle required by this function's contract.
        return Err(unsafe { sqlite_error(db, "Register compatible FTS tokenizer", rc) });
    }
    Ok(())
}

fn empty_tokenizer() -> ffi::fts5_tokenizer {
    ffi::fts5_tokenizer {
        xCreate: None,
        xDelete: None,
        xTokenize: None,
    }
}

/// # Safety
/// SQLite must pass the registered `TokenizerAlias` context and a writable
/// output pointer, and must later release the instance with `delete_alias`.
unsafe extern "C" fn create_alias(
    context: *mut c_void,
    _args: *mut *const c_char,
    _arg_count: c_int,
    output: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    if context.is_null() || output.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    // SAFETY: `context` is the boxed `TokenizerAlias` registered above and
    // remains owned by SQLite until `drop_alias` runs.
    let alias = unsafe { &*context.cast::<TokenizerAlias>() };
    let mut base = ptr::null_mut();
    // SAFETY: The base tokenizer and context came from SQLite's own
    // `xFindTokenizer` call. Passing no arguments selects its defaults.
    let rc = unsafe { (alias.create)(alias.base_context, ptr::null_mut(), 0, &mut base) };
    if rc != ffi::SQLITE_OK {
        return rc;
    }
    let instance = Box::new(TokenizerInstance {
        base,
        delete: alias.delete,
        tokenize: alias.tokenize,
    });
    // SAFETY: `output` was checked above and SQLite owns the returned
    // tokenizer until it invokes `delete_alias`.
    unsafe { *output = Box::into_raw(instance).cast() };
    ffi::SQLITE_OK
}

/// # Safety
/// `tokenizer` must be null or an instance returned by `create_alias`, and
/// SQLite must call this at most once for that instance.
unsafe extern "C" fn delete_alias(tokenizer: *mut ffi::Fts5Tokenizer) {
    if tokenizer.is_null() {
        return;
    }
    // SAFETY: `create_alias` returned this exact boxed instance to SQLite.
    let instance = unsafe { Box::from_raw(tokenizer.cast::<TokenizerInstance>()) };
    // SAFETY: The base instance belongs to the base tokenizer module and is
    // released with the destructor returned by SQLite.
    unsafe { (instance.delete)(instance.base) };
}

/// # Safety
/// SQLite must provide a live alias instance, a readable `text_len` byte
/// buffer, and a callback valid for the duration of this call.
unsafe extern "C" fn tokenize_alias(
    tokenizer: *mut ffi::Fts5Tokenizer,
    context: *mut c_void,
    flags: c_int,
    text: *const c_char,
    text_len: c_int,
    callback: Option<TokenCallback>,
) -> c_int {
    if tokenizer.is_null() || text.is_null() || text_len < 0 {
        return ffi::SQLITE_MISUSE;
    }
    let Some(callback) = callback else {
        return ffi::SQLITE_MISUSE;
    };
    // SAFETY: SQLite owns this instance and keeps it alive for the callback.
    let instance = unsafe { &*tokenizer.cast::<TokenizerInstance>() };
    // SAFETY: SQLite supplies a valid input buffer of exactly `text_len`
    // bytes for the duration of this call.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), text_len as usize) };
    let Ok(value) = std::str::from_utf8(bytes) else {
        // SAFETY: The instance, buffer, context, and callback satisfy this
        // callback's contract; the base tokenizer consumes them synchronously.
        return unsafe {
            (instance.tokenize)(
                instance.base,
                context,
                flags,
                text,
                text_len,
                Some(callback),
            )
        };
    };

    let mut segment_start = 0usize;
    for (start, ch) in value.char_indices() {
        if !is_ideograph(ch) {
            continue;
        }
        if start > segment_start {
            // SAFETY: Both offsets come from `char_indices` over the original
            // buffer and therefore delimit an in-bounds UTF-8 segment.
            let rc = unsafe {
                tokenize_base_segment(
                    instance,
                    context,
                    flags,
                    text,
                    segment_start,
                    start,
                    callback,
                )
            };
            if rc != ffi::SQLITE_OK {
                return rc;
            }
        }
        let end = start + ch.len_utf8();
        // SAFETY: The token slice lies inside SQLite's input buffer and the
        // callback consumes it synchronously.
        let rc = unsafe {
            callback(
                context,
                0,
                text.add(start),
                (end - start) as c_int,
                start as c_int,
                end as c_int,
            )
        };
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        segment_start = end;
    }

    if segment_start < bytes.len() {
        // SAFETY: `segment_start..bytes.len()` is the remaining in-bounds
        // segment of the original input buffer.
        return unsafe {
            tokenize_base_segment(
                instance,
                context,
                flags,
                text,
                segment_start,
                bytes.len(),
                callback,
            )
        };
    }
    ffi::SQLITE_OK
}

/// # Safety
/// `text` must point to the original tokenization buffer and `start..end`
/// must be an in-bounds byte range for that buffer.
unsafe fn tokenize_base_segment(
    instance: &TokenizerInstance,
    context: *mut c_void,
    flags: c_int,
    text: *const c_char,
    start: usize,
    end: usize,
    callback: TokenCallback,
) -> c_int {
    let mut offset = OffsetCallback {
        context,
        callback,
        offset: start as c_int,
    };
    // SAFETY: `start..end` is a valid slice of the original SQLite buffer;
    // `offset_callback` only adjusts offsets before forwarding synchronously.
    unsafe {
        (instance.tokenize)(
            instance.base,
            ptr::addr_of_mut!(offset).cast(),
            flags,
            text.add(start),
            (end - start) as c_int,
            Some(offset_token),
        )
    }
}

/// # Safety
/// `context` must point to the live stack `OffsetCallback` installed by
/// `tokenize_base_segment`; the base tokenizer must invoke it synchronously.
unsafe extern "C" fn offset_token(
    context: *mut c_void,
    flags: c_int,
    token: *const c_char,
    token_len: c_int,
    start: c_int,
    end: c_int,
) -> c_int {
    if context.is_null() {
        return ffi::SQLITE_MISUSE;
    }
    // SAFETY: `tokenize_base_segment` supplies this stack context and the base
    // tokenizer calls back before that stack frame returns.
    let offset = unsafe { &*context.cast::<OffsetCallback>() };
    // SAFETY: The original callback and token buffer are owned by SQLite for
    // the duration of the enclosing tokenization call.
    unsafe {
        (offset.callback)(
            offset.context,
            flags,
            token,
            token_len,
            start + offset.offset,
            end + offset.offset,
        )
    }
}

fn is_ideograph(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x2fa1f
    )
}

/// # Safety
/// `context` must be null or the boxed `TokenizerAlias` transferred to SQLite,
/// and SQLite must call this at most once after successful registration.
unsafe extern "C" fn drop_alias(context: *mut c_void) {
    if !context.is_null() {
        // SAFETY: SQLite calls this once for the pointer transferred after a
        // successful `xCreateTokenizer` registration.
        unsafe { drop(Box::from_raw(context.cast::<TokenizerAlias>())) };
    }
}

/// # Safety
/// `db` must be a live SQLite connection handle and remain valid until the
/// prepared statement is finalized.
unsafe fn fts5_api(db: *mut ffi::sqlite3) -> Result<*mut ffi::fts5_api, String> {
    let mut stmt = ptr::null_mut();
    // SAFETY: `db` is live, the SQL string is NUL-terminated, and `stmt` is a
    // writable output pointer finalized below on successful preparation.
    let rc = unsafe {
        ffi::sqlite3_prepare_v2(
            db,
            FTS5_API_QUERY.as_ptr().cast(),
            -1,
            &mut stmt,
            ptr::null_mut(),
        )
    };
    if rc != ffi::SQLITE_OK {
        // SAFETY: `db` is the live handle required by this function's contract.
        return Err(unsafe { sqlite_error(db, "Prepare FTS5 API lookup", rc) });
    }

    let mut api = ptr::null_mut::<ffi::fts5_api>();
    // SAFETY: `stmt` is live, the pointer type is NUL-terminated, and `api`
    // remains writable until the statement is stepped.
    let bind_rc = unsafe {
        sqlite3_bind_pointer(
            stmt,
            1,
            ptr::addr_of_mut!(api).cast(),
            FTS5_API_POINTER_TYPE.as_ptr().cast(),
            None,
        )
    };
    let step_rc = if bind_rc == ffi::SQLITE_OK {
        // SAFETY: `stmt` is a successfully prepared live statement.
        unsafe { ffi::sqlite3_step(stmt) }
    } else {
        bind_rc
    };
    // SAFETY: `stmt` is live and must be finalized exactly once.
    let finalize_rc = unsafe { ffi::sqlite3_finalize(stmt) };

    if step_rc != ffi::SQLITE_ROW {
        // SAFETY: `db` is the live handle required by this function's contract.
        return Err(unsafe { sqlite_error(db, "Read FTS5 API", step_rc) });
    }
    if finalize_rc != ffi::SQLITE_OK {
        // SAFETY: `db` is the live handle required by this function's contract.
        return Err(unsafe { sqlite_error(db, "Finalize FTS5 API lookup", finalize_rc) });
    }
    if api.is_null() {
        return Err("SQLite FTS5 API is unavailable".to_string());
    }
    Ok(api)
}

/// # Safety
/// `db` must be a live SQLite connection handle for the duration of the error
/// lookup.
unsafe fn sqlite_error(db: *mut ffi::sqlite3, context: &str, code: c_int) -> String {
    // SAFETY: SQLite guarantees that `sqlite3_errmsg` returns a valid
    // NUL-terminated string for a live database handle.
    let detail = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)) }.to_string_lossy();
    format!("{}: {} ({})", context, detail, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatible_tokenizer_ignores_private_schema_argument() {
        let conn = Connection::open_in_memory().unwrap();
        register_compatible_tokenizer(&conn).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE docs USING fts5(
                 body,
                 tokenize = 'MMFtsTokenizer disable_pinyin'
             );
             INSERT INTO docs(body) VALUES ('DeepSeek works'), ('数据库开源');",
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM docs WHERE docs MATCH 'deepseek'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM docs WHERE docs MATCH '开源'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(ideograph_match_query(" 开源 "), Some("开源"));
        assert_eq!(ideograph_match_query("DeepSeek"), None);
    }
}

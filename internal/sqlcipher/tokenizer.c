// Registers the upstream FTS tokenizer name as an alias of SQLite's built-in
// unicode61 tokenizer. Upstream FTS tables declare it in their schema, so
// FTS5 refuses MATCH on them until a tokenizer with that name exists.
#include <stdlib.h>
#include <string.h>
#include "sqlite3.h"

static const char *tokenizer_name = "MMFtsTokenizer";

typedef struct {
	void *base_ctx;
	fts5_tokenizer base;
} alias_t;

static int alias_create(void *ctx, const char **args, int nargs, Fts5Tokenizer **out) {
	alias_t *a = (alias_t *)ctx;
	(void)args;
	(void)nargs;
	// Upstream passes its own options; unicode61 defaults match its CJK split.
	return a->base.xCreate(a->base_ctx, NULL, 0, out);
}

// One registered context per connection; all instances share its base
// callbacks, so delete and tokenize forward through the global table below.
static fts5_tokenizer unicode61;

static void alias_delete(Fts5Tokenizer *t) { unicode61.xDelete(t); }

static int alias_tokenize(Fts5Tokenizer *t, void *ctx, int flags, const char *text, int n,
                          int (*cb)(void *, int, const char *, int, int, int)) {
	return unicode61.xTokenize(t, ctx, flags, text, n, cb);
}

static fts5_api *fts5_api_of(sqlite3 *db) {
	fts5_api *api = NULL;
	sqlite3_stmt *stmt = NULL;
	if (sqlite3_prepare_v2(db, "SELECT fts5(?1)", -1, &stmt, NULL) != SQLITE_OK) return NULL;
	sqlite3_bind_pointer(stmt, 1, (void *)&api, "fts5_api_ptr", NULL);
	sqlite3_step(stmt);
	sqlite3_finalize(stmt);
	return api;
}

int tg_register_tokenizer(sqlite3 *db) {
	fts5_api *api = fts5_api_of(db);
	if (api == NULL) return SQLITE_ERROR;
	void *existing_ctx = NULL;
	fts5_tokenizer existing;
	if (api->xFindTokenizer(api, tokenizer_name, &existing_ctx, &existing) == SQLITE_OK) return SQLITE_OK;

	alias_t *a = malloc(sizeof(alias_t));
	if (a == NULL) return SQLITE_NOMEM;
	int rc = api->xFindTokenizer(api, "unicode61", &a->base_ctx, &a->base);
	if (rc != SQLITE_OK) {
		free(a);
		return rc;
	}
	unicode61 = a->base;
	fts5_tokenizer alias = {alias_create, alias_delete, alias_tokenize};
	rc = api->xCreateTokenizer(api, tokenizer_name, a, &alias, free);
	if (rc != SQLITE_OK) free(a);
	return rc;
}

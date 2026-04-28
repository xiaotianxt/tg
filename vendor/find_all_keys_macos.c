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

#define MAX_KEYS 256
#define KEY_SIZE 32
#define SALT_SIZE 16
#define HEX_PATTERN_LEN 96
#define CHUNK_SIZE (2 * 1024 * 1024)
#define MAX_DBS 256

typedef struct {
    char key_hex[65];
    char salt_hex[33];
} key_entry_t;

static char g_db_salts[MAX_DBS][33];
static char g_db_names[MAX_DBS][256];
static int g_db_count = 0;

static int is_hex_char(unsigned char c) {
    return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

static pid_t find_telegram_pid(void) {
    FILE *fp = popen("pgrep -x Telegram", "r");
    if (!fp) return -1;
    char buf[64];
    pid_t pid = -1;
    if (fgets(buf, sizeof(buf), fp))
        pid = atoi(buf);
    pclose(fp);
    return pid;
}

static int read_db_salt(const char *path, char *salt_hex_out) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    unsigned char header[16];
    if (fread(header, 1, 16, f) != 16) { fclose(f); return -1; }
    fclose(f);
    if (memcmp(header, "SQLite format 3", 15) == 0) return -1;
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
    if (read_db_salt(fpath, salt) != 0) return 0;

    strcpy(g_db_salts[g_db_count], salt);
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

static int scan_db_storage(const char *base_path) {
    DIR *dir = opendir(base_path);
    if (!dir) return -1;

    struct dirent *ent;
    while ((ent = readdir(dir)) != NULL) {
        if (ent->d_name[0] == '.') continue;
        char storage_path[1024];
        snprintf(storage_path, sizeof(storage_path),
            "%s/%s/db_storage", base_path, ent->d_name);
        struct stat st;
        if (stat(storage_path, &st) == 0 && S_ISDIR(st.st_mode)) {
            nftw(storage_path, nftw_collect_db, 20, FTW_PHYS);
        }
    }
    closedir(dir);
    return 0;
}

int main(int argc, char *argv[]) {
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
        const char *known_paths[] = {
            "/Library/Containers/com.telegram.xinTelegram/Data/Documents/xtelegram_files",
            "/Library/Containers/com.telegram.xinTelegram/Data/Library/Application Support/com.telegram.xinTelegram",
            NULL
        };
        for (int p = 0; known_paths[p]; p++) {
            char base[1024];
            snprintf(base, sizeof(base), "%s%s", home, known_paths[p]);

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

                /* Also check for versioned subdirectories (new Telegram path) */
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

    /* Scan memory for x' patterns */
    printf("\nScanning Telegram process memory for keys...\n");
    key_entry_t keys[MAX_KEYS];
    int key_count = 0;
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

                    for (size_t i = 0; i + HEX_PATTERN_LEN + 3 < dc; i++) {
                        if (buf[i] == 'x' && buf[i + 1] == '\'') {
                            int valid = 1;
                            for (int j = 0; j < HEX_PATTERN_LEN; j++) {
                                if (!is_hex_char(buf[i + 2 + j])) { valid = 0; break; }
                            }
                            if (!valid) continue;
                            if (buf[i + 2 + HEX_PATTERN_LEN] != '\'') continue;

                            char key_hex[65], salt_hex[33];
                            memcpy(key_hex, buf + i + 2, 64);
                            key_hex[64] = '\0';
                            memcpy(salt_hex, buf + i + 2 + 64, 32);
                            salt_hex[32] = '\0';

                            for (int j = 0; key_hex[j]; j++)
                                if (key_hex[j] >= 'A' && key_hex[j] <= 'F')
                                    key_hex[j] += 32;
                            for (int j = 0; salt_hex[j]; j++)
                                if (salt_hex[j] >= 'A' && salt_hex[j] <= 'F')
                                    salt_hex[j] += 32;

                            int dup = 0;
                            for (int k = 0; k < key_count; k++) {
                                if (strcmp(keys[k].key_hex, key_hex) == 0 &&
                                    strcmp(keys[k].salt_hex, salt_hex) == 0) {
                                    dup = 1; break;
                                }
                            }
                            if (dup) continue;

                            if (key_count < MAX_KEYS) {
                                strcpy(keys[key_count].key_hex, key_hex);
                                strcpy(keys[key_count].salt_hex, salt_hex);
                                key_count++;
                            }
                        }
                    }
                    mach_vm_deallocate(mach_task_self(), data, dc);
                }
                if (cs > HEX_PATTERN_LEN + 3)
                    ca += cs - (HEX_PATTERN_LEN + 3);
                else
                    ca += cs;
            }
        }
        addr += size;
    }

    printf("Scan complete: %zuMB scanned, %d regions, %d unique keys\n",
           total_scanned / 1024 / 1024, region_count, key_count);

    /* Match keys to DBs and save JSON */
    int matched = 0;
    const char *out_path = "all_keys.json";
    FILE *fp = fopen(out_path, "w");
    if (fp) {
        fprintf(fp, "{\n");
        int first = 1;
        for (int i = 0; i < key_count; i++) {
            const char *db = NULL;
            for (int j = 0; j < g_db_count; j++) {
                if (strcmp(keys[i].salt_hex, g_db_salts[j]) == 0) {
                    db = g_db_names[j];
                    matched++;
                    break;
                }
            }
            if (!db) continue;
            fprintf(fp, "%s  \"%s\": {\"enc_key\": \"%s\"}",
                first ? "" : ",\n", db, keys[i].key_hex);
            first = 0;
        }
        fprintf(fp, "\n}\n");
        fclose(fp);
        printf("Saved %d keys to %s, matched %d/%d\n", key_count, out_path, matched, key_count);
    }

    return 0;
}

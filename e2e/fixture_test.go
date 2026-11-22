package e2e

import (
	"bytes"
	"compress/zlib"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
	"unicode"

	"github.com/xiaotianxt/tg/internal/sqlcipher"
)

// Fixture is one encrypted source directory shared by every test. Tests that
// need a variant (other keys, a media cache) build it next to their own temp
// dir and never modify this one.
type Fixture struct {
	Root  string // temp root holding everything below
	DBDir string // <account dir>/db_storage
	Keys  string // all_keys.json, mode 0600
	// Keys per relative database path, for tests that write variant key files.
	KeyHex map[string]string
}

// The account directory name carries the signed-in account id (`me`).
const (
	ownerID     = "gh_e2e_owner"
	accountDir  = ownerID + "_a1b2"
	imageID     = "0123456789abcdef0123456789abcdef"
	groupImage  = "fedcba9876543210fedcba9876543210"
	missingID   = "ffffffffffffffffffffffffffffffff"
	stickerMD5  = "aabbccddeeff00112233445566778899"
	voiceLocal  = 4
	amrLocal    = 99
	alice       = "tgid_alice"
	bob         = "tgid_bob"
	carol       = "tgid_carol"
	dave        = "tgid_dave"
	eve         = "tgid_eve"
	alicia      = "tgid_alicia"
	unknownChat = "tgid_unknown"
	team        = "team@chatroom"
	quiet       = "quiet@chatroom"
	news        = "gh_news"
)

var (
	fixtureOnce sync.Once
	fixtureVal  *Fixture
	fixtureErr  error
)

func fixture() *Fixture {
	fixtureOnce.Do(func() { fixtureVal, fixtureErr = buildFixture() })
	if fixtureErr != nil {
		panic("build fixture: " + fixtureErr.Error())
	}
	return fixtureVal
}

func cleanupFixture() {
	if fixtureVal != nil {
		os.RemoveAll(fixtureVal.Root)
	}
}

// ts is a fixture time in UTC; tests run tg with TZ=UTC.
func ts(value string) int64 {
	t, err := time.Parse("2006-01-02 15:04:05", value)
	if err != nil {
		panic(err)
	}
	return t.Unix()
}

// masked decodes a dictionary entry the way tg's own source keeps upstream
// schema names out of plain text.
func masked(encoded ...byte) string {
	out := make([]byte, len(encoded))
	for i, b := range encoded {
		out[i] = b ^ 0x5a
	}
	return string(out)
}

var (
	colBody   = masked(55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46)
	colMarker = masked(13, 25, 30, 24, 5, 25, 14, 5, 55, 63, 41, 41, 59, 61, 63, 5, 57, 53, 52, 46, 63, 52, 46)
	colSender = masked(40, 63, 59, 54, 5, 41, 63, 52, 62, 63, 40, 5, 51, 62)
	colPacked = masked(42, 59, 57, 49, 63, 62, 5, 51, 52, 60, 53, 5, 62, 59, 46, 59)
	// Account files directory under ~/Documents (media cache layout).
	accountFilesDir = masked(34, 45, 63, 57, 50, 59, 46, 5, 60, 51, 54, 63, 41)
)

func msgTable(username string) string {
	sum := md5.Sum([]byte(username))
	return "Msg_" + hex.EncodeToString(sum[:])
}

// ---------------------------------------------------------------------------
// Database writing

// The binding stores an empty Go string as NULL, so SQL that needs ” wraps
// the parameter in COALESCE.
type db struct {
	conn *sqlcipher.Conn
	err  error
}

func (d *db) exec(sql string, args ...any) {
	if d.err != nil {
		return
	}
	if len(args) == 0 {
		d.err = d.conn.Exec(sql)
		if d.err != nil {
			d.err = fmt.Errorf("%s: %w", firstLine(sql), d.err)
		}
		return
	}
	st, err := d.conn.Prepare(sql)
	if err != nil {
		d.err = fmt.Errorf("%s: %w", firstLine(sql), err)
		return
	}
	defer st.Finalize()
	if err := st.Bind(args...); err != nil {
		d.err = err
		return
	}
	if _, err := st.Step(); err != nil {
		d.err = fmt.Errorf("%s: %w", firstLine(sql), err)
	}
}

func firstLine(s string) string {
	s = strings.TrimSpace(s)
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		return s[:i]
	}
	return s
}

type fixtureBuilder struct {
	dbDir  string
	keyHex map[string]string
	seed   byte
}

// create opens a new encrypted database with its own key and salt.
func (b *fixtureBuilder) create(rel string, fill func(*db)) error {
	b.seed++
	key := bytes.Repeat([]byte{b.seed}, 32)
	salt := bytes.Repeat([]byte{b.seed + 0x80}, 16)
	path := filepath.Join(b.dbDir, rel)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	spec := []byte("x'" + hex.EncodeToString(key) + hex.EncodeToString(salt) + "'")
	conn, err := sqlcipher.Open(path, sqlcipher.OpenOptions{Key: spec})
	if err != nil {
		return fmt.Errorf("%s: %w", rel, err)
	}
	d := &db{conn: conn}
	fill(d)
	if cerr := conn.Close(); d.err == nil {
		d.err = cerr
	}
	if d.err != nil {
		return fmt.Errorf("%s: %w", rel, d.err)
	}
	b.keyHex[rel] = hex.EncodeToString(key)
	return nil
}

func buildFixture() (*Fixture, error) {
	root, err := os.MkdirTemp("", "tg-e2e-fixture-")
	if err != nil {
		return nil, err
	}
	b := &fixtureBuilder{
		dbDir:  filepath.Join(root, accountDir, "db_storage"),
		keyHex: map[string]string{},
	}
	steps := []struct {
		rel  string
		fill func(*db)
	}{
		{"contact/contact.db", fillContacts},
		{"session/session.db", fillSessions},
		{"message/message_0.db", fillShard0},
		{"message/message_1.db", fillShard1},
		{"message/message_fts.db", fillFTS},
		{"message/media_0.db", fillVoices},
		{"message/message_resource.db", fillResources},
		{"message/biz_message_0.db", fillBiz},
	}
	for _, step := range steps {
		if err := b.create(step.rel, step.fill); err != nil {
			os.RemoveAll(root)
			return nil, err
		}
	}
	keys := filepath.Join(root, "all_keys.json")
	if err := writeKeys(keys, b.keyHex); err != nil {
		os.RemoveAll(root)
		return nil, err
	}
	return &Fixture{Root: root, DBDir: b.dbDir, Keys: keys, KeyHex: b.keyHex}, nil
}

// writeKeys writes an all_keys.json the way `tg keys` does: mode 0600.
func writeKeys(path string, keyHex map[string]string) error {
	doc := map[string]map[string]string{}
	for rel, key := range keyHex {
		doc[rel] = map[string]string{"enc_key": key}
	}
	data, err := json.Marshal(doc)
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0o600)
}

// ---------------------------------------------------------------------------
// Encodings found in source rows

func varint(v uint64) []byte {
	var out []byte
	for v >= 0x80 {
		out = append(out, byte(v)|0x80)
		v >>= 7
	}
	return append(out, byte(v))
}

func pbVarint(field int, v uint64) []byte {
	return append(varint(uint64(field)<<3), varint(v)...)
}

func pbBytes(field int, data []byte) []byte {
	out := varint(uint64(field)<<3 | 2)
	out = append(out, varint(uint64(len(data)))...)
	return append(out, data...)
}

func cat(parts ...[]byte) []byte { return bytes.Join(parts, nil) }

func packedImage(name string) []byte {
	return cat(pbVarint(1, 1), pbBytes(3, cat(pbVarint(1, 90), pbVarint(2, 120), pbBytes(4, []byte(name)))))
}

func packedVoiceText(text string) []byte {
	return cat(pbVarint(1, 1), pbBytes(5, cat(pbVarint(1, 7), pbBytes(2, []byte(text)))))
}

func packedVideo(name string, width, height uint64) []byte {
	return cat(pbVarint(1, 1), pbBytes(4, cat(pbVarint(4, height), pbVarint(5, width), pbBytes(8, []byte(name)))))
}

func roomCards(cards ...[2]string) []byte {
	var out []byte
	for _, c := range cards {
		out = append(out, pbBytes(1, cat(pbBytes(1, []byte(c[0])), pbBytes(2, []byte(c[1]))))...)
	}
	return out
}

// zstdRaw wraps text in a single-segment zstd frame with one raw block, the
// simplest frame a conforming decoder accepts.
func zstdRaw(text string) []byte {
	if len(text) > 255 {
		panic("zstdRaw supports up to 255 bytes")
	}
	n := len(text)
	block := uint32(1) | uint32(n)<<3 // last block, raw
	return cat([]byte{0x28, 0xb5, 0x2f, 0xfd, 0x20, byte(n)},
		[]byte{byte(block), byte(block >> 8), byte(block >> 16)}, []byte(text))
}

func zlibBytes(text string) []byte {
	var buf bytes.Buffer
	w := zlib.NewWriter(&buf)
	w.Write([]byte(text))
	w.Close()
	return buf.Bytes()
}

// nativeVoice is a voice blob in the client's native codec, with the leading
// byte the client stores before the header.
func nativeVoice(payload string) []byte {
	header := []byte("#!SI" + "LK_V3")
	return cat([]byte{0x02}, header, []byte(payload))
}

// spaceIdeographs splits CJK ideographs into separate tokens, the way the
// upstream tokenizer indexes them.
func spaceIdeographs(s string) string {
	var b strings.Builder
	for _, r := range s {
		if unicode.Is(unicode.Han, r) {
			b.WriteRune(' ')
			b.WriteRune(r)
			b.WriteRune(' ')
		} else {
			b.WriteRune(r)
		}
	}
	return b.String()
}

// ---------------------------------------------------------------------------
// contact/contact.db

func fillContacts(d *db) {
	d.exec(`CREATE TABLE contact(
		id INTEGER PRIMARY KEY, username TEXT, local_type INTEGER, alias TEXT,
		encrypt_username TEXT, flag INTEGER, delete_flag INTEGER, verify_flag INTEGER,
		remark TEXT, remark_quan_pin TEXT, remark_pin_yin_initial TEXT, nick_name TEXT,
		pin_yin_initial TEXT, quan_pin TEXT, big_head_url TEXT, small_head_url TEXT,
		head_img_md5 TEXT, chat_room_notify INTEGER, is_in_chat_room INTEGER,
		description TEXT, extra_buffer BLOB, chat_room_type INTEGER)`)
	d.exec(`CREATE TABLE stranger(
		id INTEGER PRIMARY KEY, username TEXT, local_type INTEGER, alias TEXT,
		flag INTEGER, delete_flag INTEGER, verify_flag INTEGER, remark TEXT, nick_name TEXT)`)
	d.exec(`CREATE TABLE chat_room(id INTEGER PRIMARY KEY, username TEXT, owner TEXT, ext_buffer BLOB)`)
	d.exec(`CREATE TABLE chatroom_member(room_id INTEGER, member_id INTEGER,
		CONSTRAINT room_member UNIQUE(room_id, member_id))`)
	contacts := []struct {
		id                               int
		username                         string
		localType, flag, deleted, verify int
		alias, remark, nick              string
	}{
		{1, alice, 1, 3, 0, 0, "alice_a", "爱丽丝", "Alice Zhang"},
		{2, bob, 1, 3, 0, 0, "", "", "Bob"},
		{3, carol, 3, 0, 0, 0, "", "卡罗", "Carol"},
		{4, ownerID, 1, 1, 0, 0, "", "", "Owner"},
		{5, team, 2, 0, 0, 0, "", "", "项目群"},
		{6, quiet, 2, 0, 0, 0, "", "", "Quiet Room"},
		{7, dave, 1, 3, 0, 0, "", "", "Dave"},
		{8, eve, 3, 0, 0, 0, "", "", "Eve"},
		{9, news, 1, 3, 0, 24, "", "", "News Feed"},
		{10, alicia, 1, 3, 0, 0, "", "", "Alicia"},
	}
	for _, c := range contacts {
		d.exec(`INSERT INTO contact(id, username, local_type, alias, flag, delete_flag, verify_flag, remark, nick_name)
			VALUES (?, ?, ?, COALESCE(?, ''), ?, ?, ?, COALESCE(?, ''), COALESCE(?, ''))`,
			c.id, c.username, c.localType, c.alias, c.flag, c.deleted, c.verify, c.remark, c.nick)
	}
	// Dave is also cached as a stranger, which excludes him from friends.
	d.exec(`INSERT INTO stranger(id, username, local_type, alias, flag, delete_flag, verify_flag, remark, nick_name)
		VALUES (1, ?, 0, '', 0, 0, 0, '', 'Dave')`, dave)
	d.exec(`INSERT INTO chat_room(id, username, owner, ext_buffer) VALUES (1, ?, ?, ?)`, team, alice,
		roomCards([2]string{alice, "Alice在群里"}, [2]string{carol, "小C"}, [2]string{ownerID, "群主"}))
	d.exec(`INSERT INTO chat_room(id, username, owner, ext_buffer) VALUES (2, ?, ?, ?)`, quiet, bob, []byte{})
	for _, member := range []int{1, 2, 3, 4, 7, 8} {
		d.exec(`INSERT INTO chatroom_member(room_id, member_id) VALUES (1, ?)`, member)
	}
	for _, member := range []int{1, 2} {
		d.exec(`INSERT INTO chatroom_member(room_id, member_id) VALUES (2, ?)`, member)
	}
}

// ---------------------------------------------------------------------------
// session/session.db

func fillSessions(d *db) {
	d.exec(`CREATE TABLE SessionTable(
		username TEXT PRIMARY KEY, type INTEGER, unread_count INTEGER,
		unread_first_msg_srv_id INTEGER, unread_first_pat_msg_local_id INTEGER,
		unread_first_pat_msg_sort_seq INTEGER, is_hidden INTEGER, summary TEXT, draft TEXT,
		status INTEGER, last_timestamp INTEGER, sort_timestamp INTEGER,
		last_clear_unread_timestamp INTEGER, last_msg_locald_id INTEGER, last_msg_type INTEGER,
		last_msg_sub_type INTEGER, last_msg_sender TEXT, last_sender_display_name TEXT,
		last_msg_ext_type INTEGER)`)
	d.exec(`CREATE TABLE SessionUnreadListTable_1(username_id INTEGER, server_id INTEGER, create_time INTEGER,
		CONSTRAINT _NameId_ServerId PRIMARY KEY(username_id, server_id))`)
	d.exec(`CREATE TABLE Name2Id(user_name TEXT PRIMARY KEY)`)
	sessions := []struct {
		username     string
		unread       int
		last, sorted int64
	}{
		{alice, 2, ts("2024-06-02 11:30:00"), ts("2024-06-02 11:30:00")},
		{bob, 0, ts("2024-06-03 08:27:00"), 0},
		{team, 5, ts("2024-06-04 12:05:00"), ts("2024-06-04 12:05:00")},
		{news, 1, 0, ts("2024-05-20 08:00:00")},
		{quiet, 0, ts("2024-04-01 09:00:00"), ts("2024-04-01 09:00:00")},
		{alicia, 0, ts("2024-03-01 09:00:00"), ts("2024-03-01 09:00:00")},
		{unknownChat, 3, ts("2024-02-01 09:00:00"), ts("2024-02-01 09:00:00")},
		// Never active: listed nowhere, even with unread messages.
		{"tgid_ghost", 4, 0, 0},
		{"", 1, ts("2024-06-01 00:00:00"), 0},
	}
	for _, s := range sessions {
		d.exec(`INSERT INTO SessionTable(username, type, unread_count, last_timestamp, sort_timestamp, summary)
			VALUES (COALESCE(?, ''), 0, ?, ?, ?, 'not shown')`, s.username, s.unread, s.last, s.sorted)
	}
}

// ---------------------------------------------------------------------------
// message shards

type msg struct {
	localID   int64
	localType int64
	at        int64
	sender    string // username; empty means no sender row
	body      any    // string or []byte
	marker    any    // compression marker or nil
	packed    []byte
}

func createShard(d *db, names []string) map[string]int {
	d.exec(`CREATE TABLE Name2Id(user_name TEXT PRIMARY KEY, is_session INTEGER)`)
	ids := map[string]int{}
	for i, name := range names {
		d.exec(`INSERT INTO Name2Id(rowid, user_name, is_session) VALUES (?, ?, 1)`, i+1, name)
		ids[name] = i + 1
	}
	return ids
}

func insertMessages(d *db, ids map[string]int, session string, messages []msg) {
	table := msgTable(session)
	d.exec(fmt.Sprintf(`CREATE TABLE %s(
		local_id INTEGER PRIMARY KEY AUTOINCREMENT, server_id INTEGER, local_type INTEGER,
		sort_seq INTEGER, %s INTEGER, create_time INTEGER, status INTEGER,
		upload_status INTEGER, download_status INTEGER, server_seq INTEGER,
		origin_source INTEGER, source TEXT, %s TEXT, compress_content TEXT,
		%s BLOB, %s INTEGER DEFAULT NULL, WCDB_CT_source INTEGER DEFAULT NULL)`,
		table, colSender, colBody, colPacked, colMarker))
	d.exec(fmt.Sprintf(`CREATE INDEX %s_SORTSEQ ON %s(sort_seq)`, table, table))
	for _, m := range messages {
		sender := 0
		if m.sender != "" {
			id, ok := ids[m.sender]
			if !ok {
				panic("sender missing from Name2Id: " + m.sender)
			}
			sender = id
		}
		d.exec(fmt.Sprintf(`INSERT INTO %s(local_id, server_id, local_type, sort_seq, %s, create_time,
			status, %s, %s, %s) VALUES (?, ?, ?, ?, ?, ?, 2, ?, ?, ?)`, table, colSender, colBody, colMarker, colPacked),
			m.localID, 1000+m.localID, m.localType, m.at*1000, sender, m.at, m.body, m.marker, m.packed)
	}
}

const (
	appType = 49
	// App messages carry their subtype in the high 32 bits.
	fileAppType     = 6<<32 | appType
	linkAppType     = 5<<32 | appType
	transferAppType = 2000<<32 | appType
)

func aliceDayOne() []msg {
	return []msg{
		{1, 1, ts("2024-06-01 09:00:00"), alice, "早上好，今天吃饭了吗？", nil, nil},
		{2, 1, ts("2024-06-01 09:00:30"), ownerID, "还没有，一起去吃饭吧", nil, nil},
		{3, 3, ts("2024-06-01 09:01:10"), alice,
			`<msg><img aeskey="cafebabe" cdnthumbwidth="120" cdnthumbheight="90" length="2048"/></msg>`, nil, packedImage(imageID)},
		{voiceLocal, 34, ts("2024-06-01 09:05:00"), alice, `<msg><voicemsg endflag="1" voicelength="7000"/></msg>`, nil, packedVoiceText("这是语音转文字")},
		{5, fileAppType, ts("2024-06-01 09:06:00"), ownerID,
			`<?xml version="1.0"?><msg><appmsg><title>report.pdf</title><type>6</type><appattach><totallen>1536</totallen><fileext>pdf</fileext></appattach></appmsg></msg>`, nil, nil},
		{6, appType, ts("2024-06-01 09:10:00"), alice,
			`<msg><appmsg><title>Rust 2024 edition</title><des>release notes</des><type>5</type><url>https://example.com/rust-2024</url></appmsg></msg>`, nil, nil},
		{7, 48, ts("2024-06-01 09:12:00"), alice,
			`<msg><location x="31.24" y="121.49" scale="16" label="上海市黄浦区" poiname="外滩" /></msg>`, nil, nil},
		{8, 57<<32 | appType, ts("2024-06-01 09:15:00"), ownerID,
			`<msg><appmsg><title>好的，明天见</title><type>57</type><refermsg><type>1</type><svrid>1001</svrid><fromusr>tgid_alice</fromusr><chatusr>tgid_alice</chatusr><displayname>Alice Zhang</displayname><content>早上好，今天吃饭了吗？</content></refermsg></appmsg></msg>`, nil, nil},
		{9, 10000, ts("2024-06-01 09:20:00"), "", "你已添加了Alice Zhang，现在可以开始聊天了。", nil, nil},
		{10, 47, ts("2024-06-01 09:21:00"), alice, `<msg><emoji md5="` + stickerMD5 + `" len="1024" width="120" height="120"/></msg>`, nil, nil},
		{11, 1, ts("2024-06-01 09:30:00"), alice, zstdRaw("这条消息是压缩存储的 compressed zstd"), 4, nil},
		{12, 1, ts("2024-06-01 09:31:00"), ownerID, zlibBytes("zlib compressed reply"), 4, nil},
		{13, 19<<32 | appType, ts("2024-06-01 09:40:00"), alice,
			`<msg><appmsg><title>群聊的聊天记录</title><des>Bob: hi there` + "\n" + `Carol: hello back</des><type>19</type><recorditem><![CDATA[<recordinfo><title>群聊的聊天记录</title><datalist count="2"><dataitem datatype="1" dataid="a1"><sourcename>Bob</sourcename><sourcetime>2024-05-30 10:00</sourcetime><datadesc>hi there</datadesc></dataitem><dataitem datatype="1" dataid="a2"><sourcename>Carol</sourcename><sourcetime>2024-05-30 10:01</sourcetime><datadesc>hello back</datadesc></dataitem></datalist></recordinfo>]]></recorditem></appmsg></msg>`, nil, nil},
		{14, 43, ts("2024-06-01 09:45:00"), alice,
			`<msg><videomsg aeskey="videokey" cdnthumbwidth="320" cdnthumbheight="240" playlength="65" length="40960"/></msg>`, nil, packedVideo("aaaabbbbccccddddeeeeffff00001111", 320, 240)},
		{15, 10002, ts("2024-06-01 09:50:00"), "",
			`<sysmsg type="revokemsg"><revokemsg><session>tgid_alice</session><content>"Alice Zhang" 撤回了一条消息</content></revokemsg></sysmsg>`, nil, nil},
		{16, 1, ts("2024-06-01 09:55:00"), alice, "line one\nline two\twith tab", nil, nil},
	}
}

func aliceDayTwo() []msg {
	return []msg{
		{17, 1, ts("2024-06-02 10:00:00"), alice, "第二天的消息 from shard one", nil, nil},
		{18, 1, ts("2024-06-02 10:00:20"), ownerID, "收到 see you", nil, nil},
		{19, 1, ts("2024-06-02 11:30:00"), alice, "最后一条 latest message", nil, nil},
	}
}

const bobMessages = 55

func bobChat() []msg {
	var out []msg
	start := ts("2024-06-03 08:00:00")
	for i := 1; i <= bobMessages; i++ {
		sender := bob
		if i%2 == 0 {
			sender = ownerID
		}
		out = append(out, msg{int64(i), 1, start + int64(i-1)*30, sender, fmt.Sprintf("bob message %02d", i), nil, nil})
	}
	return out
}

func teamRecent() []msg {
	return []msg{
		{1, 1, ts("2024-06-04 12:00:00"), alice, "tgid_alice:\n大家好 welcome", nil, nil},
		{2, 1, ts("2024-06-04 12:00:40"), bob, "tgid_bob:\nhi all", nil, nil},
		{3, 1, ts("2024-06-04 12:01:00"), ownerID, "我是群主", nil, nil},
		{4, 3, ts("2024-06-04 12:02:00"), carol, "tgid_carol:\n<msg><img aeskey=\"groupkey\" /></msg>", nil, packedImage(groupImage)},
		{5, 10000, ts("2024-06-04 12:03:00"), "", "Bob邀请Carol加入了群聊", nil, nil},
		{6, 1, ts("2024-06-04 12:05:00"), bob, "tgid_bob:\n明天开会 meeting tomorrow", nil, nil},
	}
}

func fillShard0(d *db) {
	ids := createShard(d, []string{alice, ownerID, bob, carol, team, dave, unknownChat})
	insertMessages(d, ids, alice, aliceDayOne())
	insertMessages(d, ids, team, []msg{
		{1, 1, ts("2024-05-01 10:00:00"), dave, "tgid_dave:\nold message from dave", nil, nil},
	})
	insertMessages(d, ids, unknownChat, []msg{
		{1, 1, ts("2024-02-01 09:00:00"), unknownChat, "message without a contact row", nil, nil},
	})
}

func fillShard1(d *db) {
	// A different Name2Id order than shard 0: sender ids are per shard.
	ids := createShard(d, []string{team, bob, ownerID, alice, carol, dave, alicia})
	insertMessages(d, ids, alice, aliceDayTwo())
	insertMessages(d, ids, bob, bobChat())
	insertMessages(d, ids, team, teamRecent())
	insertMessages(d, ids, alicia, aliciaTransfer())
}

// aliciaTransfer is one transfer's life: she sends it (paysubtype 8, the
// current format, with a memo), the owner accepts it (3).
func aliciaTransfer() []msg {
	transfer := func(subtype, memo string) string {
		return `<msg><appmsg appid="" sdkver=""><title><![CDATA[转账]]></title>` +
			`<des><![CDATA[收到转账12.50元。请在手机上查看]]></des><type>2000</type>` +
			`<wcpayinfo><paysubtype>` + subtype + `</paysubtype><feedesc><![CDATA[￥12.50]]></feedesc>` +
			`<transferid><![CDATA[e2e-transfer-1]]></transferid><pay_memo><![CDATA[` + memo + `]]></pay_memo>` +
			`</wcpayinfo></appmsg></msg>`
	}
	return []msg{
		{1, transferAppType, ts("2024-03-01 08:58:00"), alicia, transfer("8", "午饭"), nil, nil},
		{2, transferAppType, ts("2024-03-01 09:00:00"), ownerID, transfer("3", ""), nil, nil},
	}
}

// ---------------------------------------------------------------------------
// message/message_fts.db

func fillFTS(d *db) {
	if d.err == nil {
		d.err = d.conn.RegisterCompatibleTokenizer()
	}
	d.exec(`CREATE TABLE name2id(username TEXT PRIMARY KEY)`)
	sessions := []string{alice, team, bob}
	for i, s := range sessions {
		d.exec(`INSERT INTO name2id(rowid, username) VALUES (?, ?)`, i+1, s)
	}
	type hit struct {
		table     int
		localID   int64
		localType int64
		session   int
		at        int64
		text      string
	}
	hits := []hit{
		{0, 1, 1, 1, ts("2024-06-01 09:00:00"), "早上好，今天吃饭了吗？"},
		{0, 2, 1, 1, ts("2024-06-01 09:00:30"), "还没有，一起去吃饭吧"},
		{0, 17, 1, 1, ts("2024-06-02 10:00:00"), "第二天的消息 from shard one"},
		{0, 19, 1, 1, ts("2024-06-02 11:30:00"), "最后一条 latest message"},
		{1, 1, 1, 2, ts("2024-06-04 12:00:00"), "大家好 welcome"},
		{1, 6, 1, 2, ts("2024-06-04 12:05:00"), "明天开会 meeting tomorrow"},
		{1, 1, 1, 3, ts("2024-06-03 08:00:00"), "bob message 01"},
		{1, 2, 1, 3, ts("2024-06-03 08:00:30"), "bob message 02"},
		{1, 3, 1, 3, ts("2024-06-03 08:01:00"), "bob message 03"},
	}
	for n := 0; n < 2; n++ {
		d.exec(fmt.Sprintf(`CREATE VIRTUAL TABLE message_fts_v4_%d USING fts5(
			tokenize = 'MMFtsTokenizer disable_pinyin', acontent,
			message_local_id UNINDEXED, sort_seq UNINDEXED, local_type UNINDEXED,
			session_id UNINDEXED, sender_id UNINDEXED, create_time UNINDEXED)`, n))
		d.exec(fmt.Sprintf(`CREATE TABLE message_fts_v4_aux_%d(message_local_id INTEGER, sort_seq INTEGER,
			session_id INTEGER, CONSTRAINT sessionId_localId_sortseq PRIMARY KEY(session_id, message_local_id, sort_seq))`, n))
	}
	for i, h := range hits {
		// The index holds one token per ideograph, as the upstream tokenizer
		// writes it; the stored content keeps the original text.
		rowid := i + 1
		d.exec(fmt.Sprintf(`INSERT INTO message_fts_v4_%d(rowid, acontent, message_local_id, sort_seq, local_type,
			session_id, sender_id, create_time) VALUES (?, ?, ?, ?, ?, ?, 0, ?)`, h.table),
			rowid, spaceIdeographs(h.text), h.localID, h.at*1000, h.localType, h.session, h.at)
		d.exec(fmt.Sprintf(`UPDATE message_fts_v4_%d_content SET c0 = ? WHERE id = ?`, h.table), h.text, rowid)
	}
}

// ---------------------------------------------------------------------------
// other source databases

func fillVoices(d *db) {
	d.exec(`CREATE TABLE TimeStamp(timestamp INTEGER)`)
	d.exec(`CREATE TABLE Name2Id(user_name TEXT PRIMARY KEY)`)
	d.exec(`CREATE TABLE VoiceInfo(chat_name_id INTEGER, create_time INTEGER, local_id INTEGER,
		svr_id INTEGER, voice_data BLOB, data_index TEXT DEFAULT '0')`)
	d.exec(`INSERT INTO Name2Id(rowid, user_name) VALUES (1, ?)`, bob)
	d.exec(`INSERT INTO Name2Id(rowid, user_name) VALUES (2, ?)`, alice)
	d.exec(`INSERT INTO VoiceInfo(chat_name_id, create_time, local_id, svr_id, voice_data) VALUES (2, ?, ?, 2004, ?)`,
		ts("2024-06-01 09:05:00"), voiceLocal, nativeVoice("native-voice-frames"))
	d.exec(`INSERT INTO VoiceInfo(chat_name_id, create_time, local_id, svr_id, voice_data) VALUES (2, ?, ?, 2099, ?)`,
		ts("2024-06-01 09:06:30"), amrLocal, []byte("#!AMR\namr-frames"))
}

func fillResources(d *db) {
	d.exec(`CREATE TABLE ChatName2Id(user_name TEXT PRIMARY KEY, update_time INTEGER)`)
	d.exec(`CREATE TABLE MessageResourceInfo(message_id INTEGER PRIMARY KEY AUTOINCREMENT, chat_id INTEGER,
		sender_id INTEGER, message_local_type INTEGER, message_create_time INTEGER,
		message_local_id INTEGER, message_svr_id INTEGER, message_origin_source INTEGER, packed_info BLOB)`)
}

// The official-account shard holds article pushes (one with two articles,
// one with a single article), a service notice, a compressed push whose raw
// body carries a tag the decoded text does not, and a plain text row. Unscoped queries skip it
// unless asked; reading the account itself finds every row.
func fillBiz(d *db) {
	ids := createShard(d, []string{news})
	item := func(title, url, digest string) string {
		return `<item><itemshowtype>0</itemshowtype><title><![CDATA[` + title + `]]></title>` +
			`<url><![CDATA[` + url + `]]></url><shorturl></shorturl><pub_time>1716076800</pub_time>` +
			`<digest><![CDATA[` + digest + `]]></digest><del_flag>0</del_flag>` +
			`<sources><source><name><![CDATA[News Feed]]></name></source></sources></item>`
	}
	push := func(items ...string) string {
		return `<msg><appmsg appid="" sdkver="0"><title><![CDATA[今日要闻]]></title><type>5</type>` +
			`<url><![CDATA[https://example.com/news/1]]></url><mmreader>` +
			fmt.Sprintf(`<category type="20" count="%d"><name><![CDATA[News Feed]]></name>`, len(items)) +
			strings.Join(items, "") + `</category><publisher><username><![CDATA[gh_news]]></username>` +
			`<nickname><![CDATA[News Feed]]></nickname></publisher></mmreader></appmsg>` +
			`<fromusername><![CDATA[gh_news]]></fromusername></msg>`
	}
	insertMessages(d, ids, news, []msg{
		{1, linkAppType, ts("2024-05-19 08:00:00"), news, push(
			item("今日要闻", "https://example.com/news/1", "第一篇摘要"),
			item("第二篇", "https://example.com/news/2", "第二篇摘要"),
		), nil, nil},
		{2, linkAppType, ts("2024-05-19 12:00:00"), news, push(
			item("午间快讯", "https://example.com/news/3", "只有一篇时显示摘要"),
		), nil, nil},
		{3, linkAppType, ts("2024-05-19 18:00:00"), news,
			zstdRaw(`<msg><appmsg><title>压缩推送</title><type>5</type><url>https://example.com/news/4</url><rawmark/></appmsg></msg>`), 4, nil},
		{4, 1, ts("2024-05-20 08:00:00"), news, "biz shard secret", nil, nil},
		// A service notice: its lines sit under template_detail; a hidden
		// key shows only its value.
		{5, linkAppType, ts("2024-05-19 15:00:00"), news,
			`<msg><appmsg appid="" sdkver="0"><title><![CDATA[支付成功]]></title><type>5</type>` +
				`<url><![CDATA[https://example.com/pay/1]]></url><mmreader>` +
				`<category type="0" count="1"><name><![CDATA[News Feed]]></name><item><title><![CDATA[支付成功]]></title>` +
				`<url><![CDATA[https://example.com/pay/1]]></url><pub_time>1716102000</pub_time></item></category>` +
				`<publisher><username><![CDATA[gh_news]]></username><nickname><![CDATA[News Feed]]></nickname></publisher>` +
				`<template_header><title><![CDATA[支付成功]]></title></template_header><template_detail><line_content>` +
				`<topline><key><word><![CDATA[付款金额]]></word></key><value><word><![CDATA[￥30.00]]></word></value></topline>` +
				`<lines><line><key><word><![CDATA[商户名称：]]></word></key><value><word><![CDATA[示例便利店]]></word></value></line>` +
				`<line><key><word><![CDATA[支付方式]]></word><hide>1</hide></key><value><word><![CDATA[零钱]]></word></value></line></lines>` +
				`</line_content></template_detail></mmreader></appmsg></msg>`, nil, nil},
	})
}

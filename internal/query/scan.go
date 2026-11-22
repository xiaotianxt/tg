package query

import (
	"cmp"
	"errors"
	"math"
	"math/bits"
	"slices"
	"strings"
	"sync"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/decode"
	"github.com/xiaotianxt/tg/internal/msgdb"
	"github.com/xiaotianxt/tg/internal/par"
	"github.com/xiaotianxt/tg/internal/resolve"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
	"github.com/xiaotianxt/tg/internal/store"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// Run validates o, reads every matching message across the message shards
// and returns the requested window, ordered by create_time in o.Order (ties
// keep shard, then table order). It is the entry point for the MCP query
// tool; the CLI prints the result with Output.Write.
func Run(st *store.Store, o Options) ([]Row, error) {
	if err := o.Validate(); err != nil {
		return nil, err
	}
	window, _ := o.window()
	shards := st.MessageShards()
	if o.Official || o.Session != "" {
		shards = st.SessionShards()
	}
	if len(shards) == 0 {
		return nil, errors.New("No message databases found in the source directory.")
	}
	ctx, err := loadContext(st, o)
	if err != nil {
		return nil, err
	}
	mode := ReadWindow
	if window == 0 || o.needsPostFilter() {
		mode = ReadWhole
	}
	s := &scan{st: st, o: o, ctx: ctx, mode: mode, window: window, jobs: par.Jobs(o.Jobs, par.QueryScanAutoCap)}
	cands, err := s.run(shards)
	if err != nil {
		return nil, err
	}
	slices.SortStableFunc(cands, func(a, b candidate) int {
		if o.Order == msgdb.Newest {
			return cmp.Compare(b.row.CreateTime, a.row.CreateTime)
		}
		return cmp.Compare(a.row.CreateTime, b.row.CreateTime)
	})
	cands = cands[min(o.Offset, len(cands)):]
	if o.Limited {
		cands = cands[:min(o.Limit, len(cands))]
	}
	out := make([]Row, len(cands))
	for i := range cands {
		out[i] = cands[i].finish()
	}
	return out, nil
}

// queryContext is what the scan needs from contacts and argument resolution.
type queryContext struct {
	book contacts.Book
	// tableSession maps a message table name to its session username.
	tableSession map[string]string
	// selected is the one table to read; "" reads every table.
	selected string
	// senders are the account usernames --sender selects; nil for everyone.
	senders []string
	account string
}

func loadContext(st *store.Store, o Options) (*queryContext, error) {
	conn, err := st.OpenOptional(contacts.DB)
	if err != nil {
		return nil, err
	}
	if conn != nil {
		defer conn.Close()
	}
	ctx := &queryContext{book: contacts.Book{}, tableSession: map[string]string{}, account: st.AccountUsername()}
	if conn != nil {
		if ctx.book, err = contacts.Load(conn, nil); err != nil {
			return nil, err
		}
	}
	for u := range ctx.book {
		ctx.tableSession[msgdb.TableName(u)] = u
	}
	var ref resolve.SessionRef
	if o.Session != "" {
		ref, err = resolve.Session(st, o.Session, ctx.book, resolve.Options{Activity: true, Jobs: o.Jobs, Loc: o.Loc})
		if err != nil {
			return nil, err
		}
		ctx.selected = msgdb.TableName(ref.Username)
		ctx.tableSession[ctx.selected] = ref.Username
	}
	if o.Sender != "" {
		var group resolve.GroupNames
		if o.Session != "" {
			if group, err = resolve.LoadGroupNames(conn, ref); err != nil {
				return nil, err
			}
		}
		filter, err := resolve.ResolveSender(st, o.Sender, ctx.book, group, o.Mode)
		if err != nil {
			return nil, err
		}
		ctx.senders = filter.Usernames
	}
	return ctx, nil
}

// ReadMode is how the planned tables are read.
type ReadMode int

const (
	// ReadWindow pushes `ORDER BY <order col> LIMIT window` down to each
	// table, so the index walk stops after the rows the result can use.
	// Valid only when every row SQL selects is kept.
	ReadWindow ReadMode = iota
	// ReadWhole reads every selected row in rowid order and sorts
	// afterwards: the query is unlimited, or filters rows after SQL
	// (decoded text, file/video media), so a LIMIT could cut matches.
	// Large tables are split into rowid ranges read in parallel.
	ReadWhole
)

// RowIDSpan is a table's inclusive rowid bounds; OK is false for an empty
// table.
type RowIDSpan struct {
	Lo, Hi int64
	OK     bool
}

// rows estimates the row count (rowids may be sparse), saturating.
func (s RowIDSpan) rows() uint64 {
	if !s.OK {
		return 0
	}
	n := uint64(s.Hi) - uint64(s.Lo) // Hi >= Lo, so this is the exact distance
	if n == math.MaxUint64 {
		return n
	}
	return n + 1
}

// Part is a table, or a half-open rowid range [Start, End) of one; an unset
// bound is open, so the first and last range of a table also cover rows
// outside the planned bounds.
type Part struct {
	Table      int
	Start, End msgdb.Bound
}

// Task is one unit of reader work: consecutive parts of one shard, read in
// order on one connection. Weight estimates its rows.
type Task struct {
	Shard  int
	Parts  []Part
	Weight uint64
}

// minChunkRows is the smallest rowid range worth its own reader; smaller
// tables are read whole and packed together.
const minChunkRows = 4096

// chunksPerReader caps ranges per reader, so readers that finish early find
// more work.
const chunksPerReader = 4

// PlanTasks cuts the tables of each shard (spans[shard][table]; a nil shard
// failed to plan and gets no tasks) into tasks in shard, table, range order.
// With ReadWhole, tables larger than one chunk become contiguous rowid
// ranges sized to give each reader several; smaller tables of a shard are
// packed together up to one chunk, so a small shard is not opened by several
// readers. Ranges are exact: every rowid falls in exactly one part.
func PlanTasks(spans [][]RowIDSpan, jobs int, mode ReadMode) []Task {
	var total uint64
	for _, shard := range spans {
		for _, s := range shard {
			total = satAdd(total, s.rows())
		}
	}
	maxParts := uint64(max(jobs, 1)) * chunksPerReader
	chunk := max(total/maxParts, minChunkRows)

	var tasks []Task
	for shard, tables := range spans {
		var packed []Part
		var packedRows uint64
		flush := func() {
			if len(packed) > 0 {
				tasks = append(tasks, Task{Shard: shard, Parts: packed, Weight: packedRows})
				packed, packedRows = nil, 0
			}
		}
		for table, span := range tables {
			rows := span.rows()
			split := mode == ReadWhole && rows > chunk
			if split || satAdd(packedRows, rows) > chunk {
				flush()
			}
			if !split {
				packed = append(packed, Part{Table: table})
				packedRows = satAdd(packedRows, rows)
				continue
			}
			parts := min(ceilDiv(rows, chunk), maxParts)
			partRows := ceilDiv(rows, parts)
			// Bounds never decrease, so the ranges stay disjoint and together
			// cover every rowid; clamping to Hi keeps each one a rowid.
			bound := func(part uint64) msgdb.Bound {
				off := satMul(part, partRows)
				if off >= uint64(span.Hi)-uint64(span.Lo) {
					return msgdb.At(span.Hi)
				}
				return msgdb.At(int64(uint64(span.Lo) + off))
			}
			for part := range parts {
				p := Part{Table: table}
				if part > 0 {
					p.Start = bound(part)
				}
				if part+1 < parts {
					p.End = bound(part + 1)
				}
				done := satMul(part, partRows)
				weight := uint64(0)
				if done < rows {
					weight = min(partRows, rows-done)
				}
				tasks = append(tasks, Task{Shard: shard, Parts: []Part{p}, Weight: weight})
			}
		}
		flush()
	}
	return tasks
}

func satAdd(a, b uint64) uint64 {
	s, carry := bits.Add64(a, b, 0)
	if carry != 0 {
		return math.MaxUint64
	}
	return s
}

func satMul(a, b uint64) uint64 {
	hi, lo := bits.Mul64(a, b)
	if hi != 0 {
		return math.MaxUint64
	}
	return lo
}

func ceilDiv(a, b uint64) uint64 { return a/b + min(a%b, 1) }

// tablePlan is a message table and the chat it belongs to.
type tablePlan struct {
	table msgdb.Table
	chat  resolve.Chat
	span  RowIDSpan
}

// shardPlan is what one shard's reads need, found once.
type shardPlan struct {
	label   string
	tables  []tablePlan
	name2id map[int64]string
	// filterSenders restricts rows to senderRowIDs, the Name2Id rowids of
	// the selected senders here. A shard without them plans no tables.
	filterSenders bool
	senderRowIDs  []int64
	err           error
}

// connPool holds idle connections to one shard; each reader takes its own,
// so each has its own read snapshot, as separate shards already do.
type connPool struct {
	mu    sync.Mutex
	conns []*sqlcipher.Conn
}

func (p *connPool) get(st *store.Store, label string) (*sqlcipher.Conn, error) {
	p.mu.Lock()
	if n := len(p.conns); n > 0 {
		c := p.conns[n-1]
		p.conns = p.conns[:n-1]
		p.mu.Unlock()
		return c, nil
	}
	p.mu.Unlock()
	return st.Open(label)
}

func (p *connPool) put(c *sqlcipher.Conn) {
	p.mu.Lock()
	p.conns = append(p.conns, c)
	p.mu.Unlock()
}

func (p *connPool) close() {
	for _, c := range p.conns {
		c.Close()
	}
}

type scan struct {
	st     *store.Store
	o      Options
	ctx    *queryContext
	mode   ReadMode
	window int
	jobs   int
}

// candidate is a matched row. Ordering uses only create_time, so a row no
// filter decoded is decoded once the final window is cut.
type candidate struct {
	row     msgdb.Row
	plan    *tablePlan
	msg     resolve.Message
	decoded bool
}

func (c *candidate) decode() {
	if !c.decoded {
		c.msg = c.plan.chat.Decode(c.row, tgtime.Minutes(1))
		c.decoded = true
	}
}

func (c *candidate) finish() Row {
	c.decode()
	return Row{
		Session: c.plan.chat.DisplayName, SessionID: c.plan.chat.Session.Username,
		Sender: c.msg.Sender, SenderID: c.msg.SenderID,
		LocalType: c.row.LocalType, CreateTime: c.row.CreateTime,
		Body: c.msg.Content, RawBody: c.row.Body,
	}
}

type partResult struct {
	rows []candidate
	err  error
	ran  bool
}

// run plans every shard, reads the planned parts on s.jobs readers, and
// returns the rows in shard, table, message order, each table cut to the
// window. Errors surface in the order a sequential read would hit them.
func (s *scan) run(shards []string) ([]candidate, error) {
	pools := make([]connPool, len(shards))
	defer func() {
		for i := range pools {
			pools[i].close()
		}
	}()
	idx := make([]int, len(shards))
	for i := range idx {
		idx[i] = i
	}
	plans := par.Map(idx, s.jobs, func(i int) shardPlan {
		label := shards[i]
		conn, err := s.st.Open(label)
		if err != nil {
			return shardPlan{label: label, err: err}
		}
		p, err := s.planShard(label, conn)
		if err != nil {
			conn.Close()
			return shardPlan{label: label, err: err}
		}
		// The planning connection serves the first reader of this shard.
		pools[i].put(conn)
		return p
	})

	spans := make([][]RowIDSpan, len(plans))
	for i, p := range plans {
		if p.err != nil {
			continue
		}
		spans[i] = make([]RowIDSpan, len(p.tables))
		for j, t := range p.tables {
			spans[i][j] = t.span
		}
	}
	tasks := PlanTasks(spans, s.jobs, s.mode)

	// Largest first, so long reads start early and short ones fill the gaps.
	schedule := make([]int, len(tasks))
	for i := range schedule {
		schedule[i] = i
	}
	slices.SortStableFunc(schedule, func(a, b int) int { return cmp.Compare(tasks[b].Weight, tasks[a].Weight) })
	results := make([][]partResult, len(tasks))
	par.Map(schedule, s.jobs, func(ti int) struct{} {
		results[ti] = s.runTask(&plans[tasks[ti].Shard], &pools[tasks[ti].Shard], tasks[ti])
		return struct{}{}
	})

	type flatPart struct {
		shard, table int
		res          partResult
	}
	var flat []flatPart
	for ti, t := range tasks {
		for pi, p := range t.Parts {
			flat = append(flat, flatPart{t.Shard, p.Table, results[ti][pi]})
		}
	}
	var out, tableRows []candidate
	fi := 0
	for si := range plans {
		if plans[si].err != nil {
			return nil, plans[si].err
		}
		for fi < len(flat) && flat[fi].shard == si {
			p := flat[fi]
			if !p.res.ran {
				panic("a task stops only after a failed part")
			}
			if p.res.err != nil {
				return nil, p.res.err
			}
			tableRows = append(tableRows, p.res.rows...)
			fi++
			if fi == len(flat) || flat[fi].shard != si || flat[fi].table != p.table {
				slices.SortFunc(tableRows, func(a, b candidate) int {
					if s.o.Order == msgdb.Newest {
						return b.row.Key.Compare(a.row.Key)
					}
					return a.row.Key.Compare(b.row.Key)
				})
				if s.window > 0 {
					tableRows = tableRows[:min(s.window, len(tableRows))]
				}
				out = append(out, tableRows...)
				tableRows = nil
			}
		}
	}
	return out, nil
}

// planShard finds the tables to read in one shard, their chats and rowid
// bounds.
func (s *scan) planShard(label string, conn *sqlcipher.Conn) (shardPlan, error) {
	p := shardPlan{label: label}
	if s.ctx.senders != nil {
		ids, err := msgdb.SenderRowIDs(conn, label, s.ctx.senders)
		if err != nil {
			return p, err
		}
		if len(ids) == 0 {
			// No message here is from the sender, so plan no tables.
			return p, nil
		}
		p.filterSenders, p.senderRowIDs = true, ids
	}
	var tables []msgdb.Table
	if s.ctx.selected != "" {
		schema, ok, err := msgdb.TableSchema(conn, s.ctx.selected)
		if err != nil {
			return p, err
		}
		if ok {
			tables = []msgdb.Table{{Name: s.ctx.selected, Schema: schema}}
		}
	} else {
		var err error
		if tables, err = msgdb.TableSchemas(conn); err != nil {
			return p, err
		}
	}
	name2id, err := msgdb.LoadName2ID(conn)
	if err != nil {
		return p, err
	}
	p.name2id = name2id
	// Chats without a contact row are named by their username, which this
	// shard's Name2Id knows; only an unknown table keeps its own name.
	var fromShard map[string]string
	for _, t := range tables {
		if _, ok := s.ctx.tableSession[t.Name]; !ok && fromShard == nil {
			fromShard = make(map[string]string, len(name2id))
			for _, u := range name2id {
				fromShard[msgdb.TableName(u)] = u
			}
		}
	}
	for _, t := range tables {
		lo, hi, ok, err := msgdb.RowIDBounds(conn, label, t.Name)
		if err != nil {
			return p, err
		}
		username, known := s.ctx.tableSession[t.Name]
		if !known {
			if username, known = fromShard[t.Name]; !known {
				username = t.Name
			}
		}
		p.tables = append(p.tables, tablePlan{
			table: t,
			chat: resolve.Chat{
				Session:     resolve.SessionRef{Username: username},
				DisplayName: s.ctx.book.Display(username, s.o.Mode),
				Account:     s.ctx.account,
				Names:       contacts.Names{Book: s.ctx.book, Mode: s.o.Mode},
				Loc:         s.o.Loc,
			},
			span: RowIDSpan{Lo: lo, Hi: hi, OK: ok},
		})
	}
	return p, nil
}

// runTask reads a task's parts in order on one pooled connection, stopping
// at the first failed part.
func (s *scan) runTask(plan *shardPlan, pool *connPool, t Task) []partResult {
	out := make([]partResult, len(t.Parts))
	conn, err := pool.get(s.st, plan.label)
	if err != nil {
		out[0] = partResult{err: err, ran: true}
		return out
	}
	defer pool.put(conn)
	for i, part := range t.Parts {
		rows, err := s.readPart(conn, plan, &plan.tables[part.Table], part)
		out[i] = partResult{rows: rows, err: err, ran: true}
		if err != nil {
			break
		}
	}
	return out
}

// readPart streams one part's rows, dropping rows the media filter rejects
// before decoding, and rows the body filters reject before keeping them.
func (s *scan) readPart(conn *sqlcipher.Conn, plan *shardPlan, tp *tablePlan, part Part) ([]candidate, error) {
	o := s.o
	so := msgdb.ScanOptions{
		Since: o.Since, Until: o.Until,
		FilterSenders: plan.filterSenders, SenderRowIDs: plan.senderRowIDs,
		BodyLike: o.RawContains, BodyLikeAny: o.Match == MatchAny,
		RowIDStart: part.Start, RowIDEnd: part.End,
		Order: o.Order,
	}
	if len(o.Has) > 0 {
		so.Where = []msgdb.Clause{{SQL: mediaClause(o.Has)}}
	}
	if s.mode == ReadWindow {
		so.Limit = s.window
	}
	var rows []candidate
	err := msgdb.ScanTable(conn, plan.label, tp.table, plan.name2id, so, func(r msgdb.Row) error {
		if len(o.Has) > 0 && !hasMedia(r, o.Has) {
			return nil
		}
		if !tp.table.Schema.HasLocalID {
			// Old tables without local_id use the rowid as the message id.
			r.LocalID, r.HasLocalID = r.RowID, true
		}
		c := candidate{row: r, plan: tp}
		if o.filtersBody() {
			c.decode()
			if !o.bodyMatches(c.msg.Content) {
				return nil
			}
		}
		rows = append(rows, c)
		return nil
	})
	return rows, err
}

func hasMedia(r msgdb.Row, has []Media) bool {
	got := decode.MediaType(r.LocalType, r.Body)
	return slices.ContainsFunc(has, func(m Media) bool { return m.String() == got })
}

// mediaClause preselects --has candidates in SQL. It is a superset for file
// and video, which hasMedia then decides from the raw row.
func mediaClause(has []Media) string {
	parts := make([]string, len(has))
	for i, m := range has {
		parts[i] = legacyMediaClause(m)
	}
	return strings.Join(parts, " OR ")
}

func legacyMediaClause(m Media) string {
	body := msgdb.BodyCol
	switch m {
	case Voice:
		return "((local_type & 4294967295) = 34)"
	case Image:
		return "((local_type & 4294967295) = 3)"
	case Sticker:
		return "((local_type & 4294967295) = 47)"
	case Video:
		return "((local_type & 4294967295) = 43 OR ((local_type & 4294967295) = 49 AND (local_type >> 32) = 51) OR " +
			"((local_type & 4294967295) = 49 AND " + body + " LIKE '%<finderFeed>%'))"
	case File:
		return "((local_type & 4294967295) = 62 OR (local_type & 4294967295) = 49 OR " +
			body + " LIKE '%<type>6</type>%' OR " + body + " LIKE '%<type>62</type>%')"
	}
	panic("unknown media type")
}

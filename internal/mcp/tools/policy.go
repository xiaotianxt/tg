package tools

import (
	"slices"
	"time"

	"github.com/xiaotianxt/tg/internal/contacts"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/msgdb"
)

const daySeconds = 86_400

// policy is the one place the owner's limits are applied. Size caps apply in
// every mode; session lists, the lookback window and remark hiding only in
// remote mode.
type policy struct {
	rules  config.RemotePolicy
	remote bool
	now    func() time.Time
}

// nameMode hides contact remarks from remote clients unless the owner
// allows them.
func (p policy) nameMode() contacts.NameMode {
	if p.remote && !p.rules.ShowContactRemarks {
		return contacts.Anonymous
	}
	return contacts.PersonalRemark
}

func (p policy) visible(username string) bool {
	if !p.remote {
		return true
	}
	return (len(p.rules.AllowSessions) == 0 || slices.Contains(p.rules.AllowSessions, username)) &&
		!slices.Contains(p.rules.DenySessions, username)
}

// floor is the earliest time a remote client may see.
func (p policy) floor() msgdb.Bound {
	if !p.remote {
		return msgdb.Bound{}
	}
	return msgdb.At(p.now().Unix() - p.rules.MaxLookbackDays*daySeconds)
}

// clampSince raises since to the lookback floor.
func (p policy) clampSince(since msgdb.Bound) msgdb.Bound {
	f := p.floor()
	if f.Set && (!since.Set || since.At < f.At) {
		return f
	}
	return since
}

// text caps one message at max_text_chars characters.
func (p policy) text(s string) string {
	if p.rules.MaxTextChars == nil {
		return s
	}
	limit := *p.rules.MaxTextChars
	n := 0
	for i := range s {
		if n == limit {
			return s[:i] + "…[truncated]"
		}
		n++
	}
	return s
}

func (p policy) maxBytes() int { return p.rules.MaxResultBytes }

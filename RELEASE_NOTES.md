## Official accounts, payment receipts and transfer details

### Official accounts are readable

Subscription articles and service notices, such as payment receipts, live in
their own databases, which tg never opened. Now:

- `tg messages <account>`, `tg export <account>` and `tg query --session
  <account>` read an official account like any chat.
- `tg query --official` also searches official-account messages when no
  session is given, for example `tg query --official --contains "¥" --since
  30d`. Without the flag, `query` still searches chats only.
- In MCP, `read_messages` reads official accounts, and `query_messages` takes
  `include_official`.
- `tg search` still cannot find these messages, because the app's own search
  index does not include them. Use `query` instead.

### Clearer app messages

- An article push lists each article's title and link, plus the summary when
  the push has only one article.
- A service notice lists its lines, for example the amount and merchant on a
  payment receipt.
- A transfer shows its amount, status and note, for example
  `[转账] ¥12.50 已收款 - 午饭`.
- A red packet shows its greeting. Red packets that used to appear as
  `[卡片]` with raw markup now read correctly.

### Fixes

- `tg query --raw-contains` now finds text inside compressed messages, which
  covers nearly every link, card, transfer and notice. Before, it never
  matched them and returned nothing without saying why.
- `tg doctor` lists the official-account databases and counts them in key
  coverage.

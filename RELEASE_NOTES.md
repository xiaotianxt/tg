## Gemini app connector now works

2.10.1 allowed only one of the three Google callback hosts that the Gemini app
registers at once, so adding tg as a custom connector still failed with "the
server rejected Google's redirect URL". `tg mcp serve` now also accepts
`oauth-redirect-sandbox` and `oauth-redirect-test` under
`googleusercontent.com`.

Enter the connector URL printed by `tg mcp status`, leave the client ID and
secret empty, and approve with your owner passphrase.

## Rejected registrations are logged

When a client registers with a redirect URI that is not allowed, the server
log now records the URIs it sent. Add one with
`tg mcp init --redirect-uri <uri>` if you trust that client.

Restart `tg mcp serve` after upgrading.

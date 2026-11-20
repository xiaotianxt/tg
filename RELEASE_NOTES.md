## Connect the Gemini app to `tg mcp serve`

Adding tg as a custom MCP connector in the Gemini app failed with "the server
rejected Google's redirect URL", and Gemini then asked for an OAuth client ID
and secret, which tg does not issue.

The remote server now accepts Gemini's callback under
`https://oauth-redirect.googleusercontent.com/` alongside ChatGPT's. Enter the
connector URL printed by `tg mcp status` and approve with your owner
passphrase; leave the client ID and secret empty.

Redirects with a query string, percent escapes, or `..` path segments are
still rejected. Restart `tg mcp serve` after upgrading.

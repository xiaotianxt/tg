## MCP tools say when to use them

ChatGPT and other MCP clients decide whether to call a tool from its
description. tg's descriptions said what each tool returns but not which
questions need it, so questions that never mention chat history ("why did he
turn the offer down?", "what has the group been discussing?") rarely reached
tg.

The server instructions and every tool description now start with when to use
them, cover those indirect questions, name the cases that should not use tg
(general knowledge, public facts, writing that needs no past conversation), and
name the chat app the history comes from. The instructions also ask the model to
cite the messages it read and to say when local history is incomplete.

After upgrading, open the connection in ChatGPT Plugins and select **Refresh**
so ChatGPT reads the new metadata.

## Name the ChatGPT connector after your data

ChatGPT does not read a connector's name and description from the server; you
type them when adding it, and the model sees them before any tool. The README
now suggests a name and a "use it when" description for that step. A connector
named only `tg` gives the model little reason to pick it.

# Corsia for VS Code

Adds **Corsia** as a model provider in VS Code's own chat: the models in Corsia's models folder appear in the chat model picker under "Corsia", the running one marked. Chat and agent mode both work (tools and tool calls go through Corsia's OpenAI-style endpoint). Picking a model that is not running switches Corsia to it; a message sent while Corsia is stopped starts it (`corsia start`). "Manage" in the model picker opens Corsia's management page.

Install (from this folder):

```
./package.sh && code --install-extension corsia-0.1.0.vsix
```

Settings: `corsia.endpoint` (default `http://127.0.0.1:7720/v1`), `corsia.daemon` (default `http://127.0.0.1:7715`), `corsia.startIfStopped` (default on).

VS Code offers models from provider extensions to individual GitHub Copilot plans (Free, Pro, Pro+), so chat needs a GitHub sign-in; the answers themselves come from Corsia on this machine.

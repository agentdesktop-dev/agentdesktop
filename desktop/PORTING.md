# Desktop UI port

This directory began as an isolated snapshot of `ui/` from
`peterj/uiexperiment` and is now the Agent Desktop native application.

The React interface retains the experiment's visual design. The Rust code under
`src-tauri/` is a thin native shell over `agentdesktop-client` and the daemon's
local API. The daemon remains the sole owner of enrollment, discovery,
reconciliation, credentials, configuration, and controller connectivity.

The controller web application remains independent under `ui/`.

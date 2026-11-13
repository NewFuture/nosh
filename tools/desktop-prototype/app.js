"use strict";

const $ = (selector) => document.querySelector(selector);
const app = $(".app");
const icon = (name) => {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.classList.add("icon");
  svg.setAttribute("aria-hidden", "true");
  const use = document.createElementNS(svg.namespaceURI, "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
};
const element = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const hosts = [
  { id: "local", name: "This computer", host: "localhost", user: "you", port: 22, remote: false, enhanced: true },
  { id: "staging", name: "staging", host: "10.0.0.8", user: "deploy", port: 22, remote: true, enhanced: true },
  { id: "production", name: "production", host: "10.0.0.12", user: "deploy", port: 22, remote: true, enhanced: false },
];
let nextId = 1;
const sessions = [];
let activeId = "staging-session";
let trustTarget = null;
let toastTimer;

function createSession(host, id) {
  const remote = host.remote;
  const session = {
    id, host, connected: true,
    path: remote ? "/srv/storefront" : "~/projects/nosh",
    entries: remote ? [
      { command: "git pull --ff-only", output: "Already up to date.", kind: "success", time: "16:04:12" },
      { command: "systemctl --user status storefront", output: "× storefront.service - Storefront API\n     Loaded: loaded; enabled\n     Active: failed (Result: exit-code)\n\nNov 11 16:04:18 staging node[1842]: Starting server…\nNov 11 16:04:18 staging node[1842]: Error: listen EADDRINUSE: address already in use :::3000\nNov 11 16:04:18 staging systemd[1]: storefront.service: Failed with result 'exit-code'.", kind: "error", exit: 3, time: "16:04:18" },
    ] : [
      { command: "git status --short", output: " M crates/nosh-shell/src/repl.rs\n?? docs/notes.txt", time: "16:02:06" },
      { command: "cargo test -p nosh-engine --locked", output: "running 24 tests\n........................\ntest result: ok. 24 passed; 0 failed", kind: "success", time: "16:02:11" },
    ],
    messages: [], pending: null, suggestion: remote ? "ss -ltnp 'sport = :3000'" : "git diff --stat",
    draft: "", assistantDraft: "",
  };
  if (remote && host.enhanced) {
    session.messages.push(
      { role: "user", text: "Why won't the storefront service start?" },
      { role: "assistant", text: "The service can't bind to port 3000 because another process is already listening. I'll check who owns the port before suggesting a change.", step: "Reviewed the last command output" },
    );
    session.pending = { command: "ss -ltnp 'sport = :3000'", reason: "Read-only inspection of listening sockets. No services will be stopped or restarted." };
  } else {
    session.messages.push({ role: "assistant", text: remote ? "You're connected to a basic SSH terminal. I can suggest commands to review and run yourself. Remote Agent execution requires an enhanced nosh session." : "A clean shell and a second pair of eyes. Ask me to explain an error or draft your next command. Nothing runs without your say-so.", step: "Session context ready · simulated" });
  }
  sessions.push(session);
  return session;
}
createSession(hosts[0], "local-session");
createSession(hosts[1], activeId);

function activeSession() { return sessions.find((session) => session.id === activeId); }
function targetName(session) { return session.host.remote ? `${session.host.user}@${session.host.name}` : "local · nosh"; }
function toast(text) {
  clearTimeout(toastTimer);
  $("#toast").textContent = text;
  $("#toast").hidden = false;
  toastTimer = setTimeout(() => { $("#toast").hidden = true; }, 4000);
}
function openDialog(id) { $(id).showModal(); }
function saveDrafts() {
  const session = activeSession();
  session.draft = $("#terminal-input").value;
  session.assistantDraft = $("#assistant-input").value;
}
function selectSession(id) {
  saveDrafts();
  activeId = id;
  render();
}
function selectHost(host) {
  const existing = sessions.find((session) => session.host.id === host.id);
  if (existing?.connected) {
    selectSession(existing.id);
  } else {
    trustTarget = host;
    $("#trust-host").textContent = `${host.user}@${host.host}:${host.port}`;
    $("#trust-capability").textContent = host.enhanced
      ? "Enhanced remote nosh session · simulated capability. A real connection must verify the remote runtime before enabling Agent tools."
      : "Basic SSH terminal. Enhanced remote Agent support is not installed on this host.";
    openDialog("#trust-dialog");
  }
}
function renderConnections() {
  $("#local-connections").replaceChildren();
  $("#remote-connections").replaceChildren();
  $("#remote-count").textContent = hosts.filter((host) => host.remote).length;
  for (const host of hosts) {
    const connected = sessions.some((session) => session.host.id === host.id && session.connected);
    const button = element("button", `connection-button${activeSession().host.id === host.id ? " active" : ""}`);
    button.append(icon(host.remote ? "server" : "laptop"));
    button.setAttribute("aria-pressed", String(activeSession().host.id === host.id));
    const text = element("span", "connection-text");
    text.append(element("strong", "", host.name), element("small", "", host.remote ? `${host.user}@${host.host}` : "~/projects/nosh"));
    button.append(text, element("span", `status-dot${connected ? "" : " offline"}`));
    button.title = `${host.name} · ${connected ? "connected" : "disconnected"} · simulated`;
    button.addEventListener("click", () => { selectHost(host); app.classList.remove("sidebar-open"); });
    $(host.remote ? "#remote-connections" : "#local-connections").append(button);
  }
}
function renderTabs() {
  $("#tabs").replaceChildren();
  for (const session of sessions) {
    const tab = element("button", "tab");
    tab.id = `tab-${session.id}`;
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-selected", String(session.id === activeId));
    tab.tabIndex = session.id === activeId ? 0 : -1;
    tab.append(icon(session.host.remote ? "server" : "terminal"), element("span", "", session.host.remote ? session.host.name : "nosh"), element("span", "tab-type", session.host.remote ? "SSH" : "LOCAL"));
    tab.addEventListener("click", () => selectSession(session.id));
    tab.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
      event.preventDefault();
      const current = sessions.indexOf(session);
      const index = event.key === "Home" ? 0 : event.key === "End" ? sessions.length - 1 : (current + (event.key === "ArrowRight" ? 1 : -1) + sessions.length) % sessions.length;
      selectSession(sessions[index].id);
      $(`#tab-${activeId}`).focus();
    });
    $("#tabs").append(tab);
  }
}
function renderTerminal() {
  const session = activeSession();
  const output = $("#terminal-output");
  output.replaceChildren();
  const welcome = element("div", "terminal-welcome");
  const intro = element("div");
  intro.append(element("strong", "", session.host.remote ? `A little closer to ${session.host.name}.` : "Less friction. More flow."), element("p", "", session.host.remote ? "SSH workspace · demo connection · no network activity" : "nosh desktop · local shell simulation"));
  welcome.append(icon(session.host.remote ? "link" : "terminal"), intro);
  output.append(welcome);
  for (const entry of session.entries) {
    const block = element("div", "terminal-block");
    const command = element("div", "terminal-command");
    command.append(element("span", "command-directory", session.host.remote ? session.host.name : "nosh"), element("span", "command-chevron", "❯"), element("span", "", entry.command), element("span", "command-time", entry.time));
    block.append(command, element("pre", `command-output ${entry.kind || ""}`, entry.output));
    if (entry.exit) block.append(element("span", "output-tag", `exit ${entry.exit} · simulated`));
    output.append(block);
  }
  $("#terminal-input").value = session.draft;
  $("#terminal-input").disabled = !session.connected;
  $("#terminal-input").placeholder = session.connected ? "Type a command…" : "Session disconnected. Reconnect from the sidebar.";
  $("#terminal-dot").classList.toggle("offline", !session.connected);
  $("#terminal-identity").textContent = session.host.remote ? `${session.host.user}@${session.host.host}` : "you@this-computer";
  $("#suggestion").hidden = !session.suggestion || !session.connected;
  $("#suggestion-code").textContent = session.suggestion || "";
  $("#terminal-scroll").scrollTop = $("#terminal-scroll").scrollHeight;
}
function insertCommand(session, command) {
  if (session.id !== activeId || !session.connected) {
    toast("This suggestion belongs to a disconnected or different session.");
    return;
  }
  const input = $("#terminal-input");
  if (input.value.trim() && input.value !== command) {
    toast("Your terminal draft is kept. Clear it before inserting a suggestion.");
    return;
  }
  session.draft = command;
  input.value = command;
  if (matchMedia("(max-width: 650px)").matches) setAssistant(false);
  input.focus();
  toast("Inserted for review. Not executed.");
}
function renderConversation() {
  const session = activeSession();
  const conversation = $("#conversation");
  conversation.replaceChildren();
  for (const message of session.messages) {
    const block = element("div", "message");
    const label = element("div", "message-label");
    if (message.role === "assistant") label.append(icon("spark"));
    label.append(document.createTextNode(message.role === "assistant" ? "NOSH" : "YOU"));
    block.append(label);
    if (message.step) {
      const step = element("div", "step-line");
      step.append(icon("check"), document.createTextNode(message.step));
      block.append(step);
    }
    block.append(element("p", message.role === "user" ? "user-bubble" : "", message.text));
    if (message.suggestion) {
      const result = element("div", "result-card");
      const insert = element("button", "", "Insert into terminal →");
      insert.disabled = !session.connected;
      insert.addEventListener("click", () => insertCommand(session, message.suggestion));
      result.append(element("p", "muted", "Draft only · review before running"), element("code", "", message.suggestion), insert);
      block.append(result);
    }
    conversation.append(block);
  }
  if (session.pending) {
    const pending = session.pending;
    const card = element("div", "approval-card");
    const header = element("div", "approval-header");
    header.append(icon("shield"), element("span", "", "Permission to inspect"), element("span", "", "READ ONLY"));
    const content = element("div", "approval-content");
    content.append(element("div", "approval-target", `${targetName(session)} · ${session.path}`), element("pre", "", pending.command), element("p", "", pending.reason));
    const actions = element("div", "approval-buttons");
    const deny = element("button", "button secondary", "Deny");
    const allow = element("button", "button primary", "Allow once");
    deny.addEventListener("click", () => resolveApproval(session, pending, false));
    allow.addEventListener("click", () => resolveApproval(session, pending, true));
    allow.disabled = !session.connected;
    actions.append(deny, allow);
    card.append(header, content, actions);
    conversation.append(card);
  }
  $("#assistant-input").value = session.assistantDraft;
  $("#assistant-input").disabled = !session.connected;
  $(".send-button").disabled = !session.connected;
  $("#assistant-scroll").scrollTop = $("#assistant-scroll").scrollHeight;
}
function render() {
  const session = activeSession();
  renderConnections();
  renderTabs();
  const parts = session.path.split("/");
  $("#project-name").textContent = parts.pop() || "/";
  $("#session-path").textContent = parts.join("/");
  $("#transport-label").textContent = session.host.remote ? session.connected ? "SSH · encrypted (demo)" : "SSH · disconnected" : "LOCAL SESSION";
  $("#disconnect-session").hidden = !session.host.remote;
  $("#disconnect-session").disabled = !session.connected;
  $("#assistant-target").textContent = targetName(session);
  $("#connection-status").textContent = session.host.remote ? `SSH ${session.connected ? "connected" : "disconnected"} · ${session.host.name}` : "Local session";
  renderTerminal();
  renderConversation();
}
function appendCommand(session, command, output, kind = "", exit = 0) {
  session.entries.push({ command, output, kind, exit, time: new Date().toLocaleTimeString("en-GB") });
}
function simulateCommand(session, command) {
  if (command === "clear") {
    session.entries = [];
  } else if (command === "pwd") {
    appendCommand(session, command, session.path);
  } else if (command === "help" || command === "#help") {
    appendCommand(session, command, "This is a UI prototype, not a real shell.\nTry: pwd, ls, git status, ss -ltnp 'sport = :3000', clear.\nUse the assistant to review a suggested command.\nNo command in this window is actually executed.");
  } else if (command === "ls" || command === "ls -la") {
    appendCommand(session, command, session.host.remote ? "app/    config/    logs/    package.json" : "assets/    crates/    docs/    tools/    Cargo.toml");
  } else if (command === "git status" || command === "git status --short") {
    appendCommand(session, command, "On branch main\nYour branch is up to date with 'origin/main'.\n\nnothing to commit, working tree clean", "success");
  } else if (command === "git diff --stat") {
    appendCommand(session, command, " crates/nosh-shell/src/repl.rs | 8 ++++++--\n 1 file changed, 6 insertions(+), 2 deletions(-)");
  } else if (command === "ss -ltnp 'sport = :3000'") {
    appendCommand(session, command, 'State   Local Address:Port   Process\nLISTEN  *:3000               node (pid=1790)\n\n[Simulated output · no host was contacted]', "success");
  } else if (command === "systemctl --user status storefront") {
    appendCommand(session, command, "× storefront.service: failed\nError: listen EADDRINUSE: address already in use :::3000\n[Simulated output]", "error", 3);
  } else {
    appendCommand(session, command, "Not executed: this prototype has no shell backend.\nUse help to see the available UI simulations.");
  }
  session.draft = "";
  renderTerminal();
}
function resolveApproval(session, pending, allowed) {
  if (session.pending !== pending || activeId !== session.id) return;
  if (allowed && !session.connected) {
    toast("Reconnect before approving an operation.");
    return;
  }
  saveDrafts();
  session.pending = null;
  if (allowed) {
    appendCommand(session, pending.command, 'State   Local Address:Port   Process\nLISTEN  *:3000               node (pid=1790)\n\n[Simulated approval and execution · no real command ran]', "success");
    session.messages.push({ role: "assistant", step: "Allowed once · simulated result", text: "A Node process (PID 1790) is listening on port 3000 in this demo. Inspect its owner and purpose before stopping it. I haven't changed or restarted anything.", suggestion: "ps -fp 1790" });
    session.suggestion = "ps -fp 1790";
  } else {
    session.messages.push({ role: "assistant", step: "Permission denied · nothing executed", text: "I won't inspect the port. You can review the suggested command and run it yourself, or continue with another question." });
  }
  renderTerminal();
  renderConversation();
}
function askAssistant(text) {
  const session = activeSession();
  if (!session.connected) { toast("This session is disconnected."); return; }
  if (session.pending) { toast("Allow or deny the pending operation before starting another request."); return; }
  saveDrafts();
  session.assistantDraft = "";
  session.messages.push({ role: "user", text });
  const isError = /error|fail|start|port|失败|错误|启动/i.test(text);
  if (isError && session.host.remote && session.host.enhanced) {
    session.messages.push({ role: "assistant", step: "Demo response · no model loaded", text: "The sample output shows that port 3000 is already in use. I can inspect the listening process without changing the service." });
    session.pending = { command: "ss -ltnp 'sport = :3000'", reason: "Read-only inspection of listening sockets. Scoped to this SSH session." };
  } else {
    const command = session.host.remote ? "ss -ltnp 'sport = :3000'" : "git diff --stat";
    session.messages.push({ role: "assistant", step: "Scripted demonstration · not a real AI response", text: session.host.remote ? "Here's a command to inspect the sample service failure. It checks which process is listening on port 3000. Review it before running; basic SSH never enables remote Agent execution." : "Here's an example next step: summarize your working-tree changes before choosing what to inspect. This is a fixed demo response, not an analysis of your actual project.", suggestion: command });
    session.suggestion = command;
  }
  renderConversation();
  renderTerminal();
}
function setAssistant(open) {
  app.classList.toggle("assistant-hidden", !open);
  $("#toggle-assistant").setAttribute("aria-expanded", String(open));
}
function renderHistory() {
  const query = $("#history-search").value.toLowerCase();
  const list = $("#history-list");
  list.replaceChildren();
  const commands = [...new Set(activeSession().entries.map((entry) => entry.command))].reverse().filter((command) => command.toLowerCase().includes(query));
  for (const command of commands) {
    const button = element("button", "history-item");
    button.append(element("code", "", command), element("span", "", "Insert ↵"));
    button.addEventListener("click", () => { $("#history-dialog").close(); insertCommand(activeSession(), command); });
    list.append(button);
  }
  if (!commands.length) list.append(element("p", "empty-state", "No commands match in this session."));
}
function openHistory() {
  $("#history-search").value = "";
  renderHistory();
  openDialog("#history-dialog");
  $("#history-search").focus();
}
function renderPalette() {
  const query = $("#palette-search").value.toLowerCase();
  const results = $("#palette-results");
  results.replaceChildren();
  const actions = [
    ...hosts.map((host) => ({ name: host.name, detail: host.remote ? `SSH · ${host.host}` : "Local", icon: host.remote ? "server" : "laptop", run: () => selectHost(host) })),
    { name: "New SSH connection", detail: "Connection", icon: "plus", run: () => openDialog("#connection-dialog") },
    { name: "Command history", detail: "Current session", icon: "history", run: openHistory },
    { name: "Settings & local model", detail: "Preferences", icon: "settings", run: () => openDialog("#settings-dialog") },
  ];
  for (const action of actions.filter((action) => `${action.name} ${action.detail}`.toLowerCase().includes(query))) {
    const button = element("button", "palette-item");
    button.append(icon(action.icon), element("span", "", action.name), element("small", "", action.detail));
    button.addEventListener("click", () => { $("#palette-dialog").close(); action.run(); });
    results.append(button);
  }
  if (!results.childElementCount) results.append(element("p", "empty-state", "No matching connections or actions."));
}
function openPalette() {
  $("#palette-search").value = "";
  renderPalette();
  openDialog("#palette-dialog");
  $("#palette-search").focus();
}

$("#terminal-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const session = activeSession();
  const command = $("#terminal-input").value.trim();
  if (command && session.connected) simulateCommand(session, command);
});
$("#assistant-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const text = $("#assistant-input").value.trim();
  if (text) askAssistant(text);
});
$("#assistant-input").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    $("#assistant-form").requestSubmit();
  }
});
document.querySelectorAll("[data-prompt]").forEach((button) => button.addEventListener("click", () => askAssistant(button.dataset.prompt)));
document.querySelectorAll("[data-close]").forEach((button) => button.addEventListener("click", () => button.closest("dialog").close()));
document.querySelectorAll("dialog").forEach((dialog) => dialog.addEventListener("click", (event) => {
  if (event.target !== dialog) return;
  const bounds = dialog.getBoundingClientRect();
  if (event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom) dialog.close();
}));
$("#insert-suggestion").addEventListener("click", () => insertCommand(activeSession(), activeSession().suggestion));
$("#dismiss-suggestion").addEventListener("click", () => {
  saveDrafts();
  activeSession().suggestion = null;
  $("#suggestion").hidden = true;
});
$("#toggle-sidebar").addEventListener("click", () => {
  if (matchMedia("(max-width: 940px)").matches) app.classList.toggle("sidebar-open");
  else app.classList.toggle("sidebar-hidden");
});
$("#toggle-assistant").addEventListener("click", () => setAssistant(app.classList.contains("assistant-hidden")));
$("#close-assistant").addEventListener("click", () => setAssistant(false));
$("#new-terminal").addEventListener("click", () => {
  const session = createSession(hosts[0], `local-${nextId++}`);
  selectSession(session.id);
  $("#terminal-input").focus();
});
$("#add-connection").addEventListener("click", () => openDialog("#connection-dialog"));
$("#add-host").addEventListener("click", () => openDialog("#connection-dialog"));
$("#connection-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const form = event.currentTarget;
  const values = new FormData(form);
  const name = values.get("name").trim();
  if (!name) { toast("Enter a connection name."); return; }
  const host = { id: `host-${nextId++}`, name, host: values.get("host").trim(), user: values.get("user").trim(), port: Number(values.get("port")), remote: true, enhanced: false };
  hosts.push(host);
  renderConnections();
  $("#connection-dialog").close();
  form.reset();
  toast("Demo connection added. Select it to preview host verification.");
});
$("#trust-connect").addEventListener("click", () => {
  if (!trustTarget) return;
  saveDrafts();
  let session = sessions.find((item) => item.host.id === trustTarget.id);
  if (session) {
    session.connected = true;
    session.messages.push({ role: "assistant", text: "A new demo transport is connected. No command was replayed. A real app must verify the remote shell state before continuing." });
    appendCommand(session, "[demo reconnect]", "Connected to a fresh simulated transport. Prior process state is not restored.");
  } else {
    session = createSession(trustTarget, `ssh-${nextId++}`);
    session.entries = [];
    session.suggestion = "pwd";
  }
  activeId = session.id;
  trustTarget = null;
  $("#trust-dialog").close();
  render();
  toast("Demo SSH connected. No network connection was made.");
});
$("#disconnect-session").addEventListener("click", () => {
  saveDrafts();
  const session = activeSession();
  session.connected = false;
  session.pending = null;
  session.messages.push({ role: "assistant", text: "SSH disconnected. Pending permissions were cleared. Reconnecting won't replay commands or restore process state. In a real disconnect, unfinished operations may have an unknown outcome." });
  render();
  toast("Demo session disconnected. Select the connection to reconnect.");
});
$("#open-history").addEventListener("click", openHistory);
$("#history-search").addEventListener("input", renderHistory);
$("#open-palette").addEventListener("click", openPalette);
$("#palette-search").addEventListener("input", renderPalette);
for (const selector of ["#open-settings", "#open-model", "#model-status", "#approval-mode"]) {
  $(selector).addEventListener("click", () => openDialog("#settings-dialog"));
}
$("#theme-toggle").addEventListener("click", () => {
  const dark = document.body.classList.toggle("dark");
  $("#theme-label").textContent = dark ? "Switch to light" : "Switch to dark";
});
$("#font-size").addEventListener("change", (event) => document.documentElement.style.setProperty("--terminal-size", `${event.target.value}px`));
$(".brand").addEventListener("click", (event) => { event.preventDefault(); selectSession("local-session"); });
document.addEventListener("keydown", (event) => {
  if (!(event.ctrlKey || event.metaKey) || event.altKey || event.isComposing) return;
  const key = event.key.toLowerCase();
  if (!["k", "r", ","].includes(key)) return;
  if (document.querySelector("dialog[open]")) return;
  event.preventDefault();
  if (key === "k") openPalette();
  else if (key === "r") openHistory();
  else openDialog("#settings-dialog");
});
if (matchMedia("(max-width: 650px)").matches) setAssistant(false);
render();

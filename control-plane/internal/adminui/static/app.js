const state = { config: null, status: "pending", token: sessionStorage.getItem("admin_token") };
const $ = (selector) => document.querySelector(selector);

function encode(bytes) {
  return btoa(String.fromCharCode(...bytes)).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}

async function digest(value) {
  return encode(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value))));
}

function randomValue(size = 32) {
  const bytes = new Uint8Array(size);
  crypto.getRandomValues(bytes);
  return encode(bytes);
}

async function signIn() {
  const verifier = randomValue(64);
  const loginState = randomValue();
  sessionStorage.setItem("pkce_verifier", verifier);
  sessionStorage.setItem("oauth_state", loginState);
  const url = new URL(state.config.authorization_endpoint);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", state.config.client_id);
  url.searchParams.set("redirect_uri", `${location.origin}/admin/`);
  url.searchParams.set("scope", state.config.scope);
  url.searchParams.set("audience", state.config.audience);
  url.searchParams.set("state", loginState);
  url.searchParams.set("code_challenge", await digest(verifier));
  url.searchParams.set("code_challenge_method", "S256");
  location.assign(url);
}

async function completeSignIn() {
  const query = new URLSearchParams(location.search);
  const code = query.get("code");
  if (!code) return;
  if (query.get("state") !== sessionStorage.getItem("oauth_state")) throw new Error("Sign-in state did not match");
  const form = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    client_id: state.config.client_id,
    redirect_uri: `${location.origin}/admin/`,
    code_verifier: sessionStorage.getItem("pkce_verifier") || "",
  });
  const response = await fetch(state.config.token_endpoint, { method: "POST", body: form });
  if (!response.ok) throw new Error("The identity provider rejected administrator sign-in");
  const tokens = await response.json();
  state.token = tokens.access_token;
  sessionStorage.setItem("admin_token", state.token);
  sessionStorage.removeItem("pkce_verifier");
  sessionStorage.removeItem("oauth_state");
  history.replaceState({}, "", "/admin/");
}

function signOut() {
  sessionStorage.clear();
  state.token = null;
  renderSession();
}

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { authorization: `Bearer ${state.token}`, ...(options.headers || {}) },
  });
  if (response.status === 401) {
    signOut();
    throw new Error("Administrator session expired. Sign in again.");
  }
  const body = await response.json();
  if (!response.ok) throw new Error(body.error?.code || "The request could not be completed");
  return body;
}

function escape(value) {
  const element = document.createElement("span");
  element.textContent = value;
  return element.innerHTML;
}

function recordTemplate(record) {
  const created = new Date(record.created_at).toLocaleString();
  const device = record.device_id ? `<span>Device ${escape(record.device_id)}</span>` : "";
  let actions = "";
  if (state.status === "pending") {
    actions = `<button class="reject" data-action="reject">Reject</button><button class="approve" data-action="approve">Approve</button>`;
  } else if (state.status === "approved" && record.device_id) {
    actions = `<button class="revoke" data-action="revoke">Revoke device</button>`;
  }
  return `<article class="record" data-enrollment="${escape(record.enrollment_id)}" data-device="${escape(record.device_id || "")}">
    <div><span class="badge">${escape(record.status)}</span><h2>${escape(record.subject)}</h2>
      <div class="meta"><span>Requested ${escape(created)}</span>${device}</div>
      <div class="fingerprint">Key ${escape(record.public_key_fingerprint)}</div>
    </div><div class="actions">${actions}</div></article>`;
}

async function load() {
  const records = await api(`/v1/admin/enrollments?status=${state.status}`);
  const items = records.enrollments || [];
  $("#records").innerHTML = items.map(recordTemplate).join("");
  $("#empty").classList.toggle("hidden", items.length !== 0);
  if (state.status === "pending") $("#pending-count").textContent = items.length;
  $("#view-title").textContent = `${state.status[0].toUpperCase()}${state.status.slice(1)} requests`;
}

async function act(button) {
  const record = button.closest(".record");
  const action = button.dataset.action;
  button.disabled = true;
  try {
    const path = action === "revoke"
      ? `/v1/admin/devices/${record.dataset.device}/revoke`
      : `/v1/admin/enrollments/${record.dataset.enrollment}/${action}`;
    await api(path, { method: "POST" });
    notify(action === "approve" ? "Device approved" : action === "reject" ? "Request rejected" : "Device revoked");
    await load();
  } catch (error) {
    notify(error.message, true);
    button.disabled = false;
  }
}

function notify(message, error = false) {
  const notice = $("#notice");
  notice.textContent = message;
  notice.classList.remove("hidden");
  notice.classList.toggle("error", error);
}

function renderSession() {
  const signedIn = Boolean(state.token);
  $("#sign-in").classList.toggle("hidden", signedIn);
  $("#sign-out").classList.toggle("hidden", !signedIn);
  $("#workspace").classList.toggle("hidden", !signedIn);
  $("#session").textContent = signedIn ? "Administrator session" : "Signed out";
  if (signedIn) load().catch((error) => notify(error.message, true));
}

async function init() {
  state.config = await fetch("/v1/admin/ui-config").then((response) => response.json());
  $("#organization").textContent = state.config.organization_name;
  await completeSignIn();
  renderSession();
  $("#sign-in").addEventListener("click", signIn);
  $("#sign-out").addEventListener("click", signOut);
  $("#refresh").addEventListener("click", () => load().catch((error) => notify(error.message, true)));
  $(".tabs").addEventListener("click", (event) => {
    const tab = event.target.closest("button[data-status]");
    if (!tab) return;
    document.querySelectorAll(".tabs button").forEach((button) => button.classList.toggle("active", button === tab));
    state.status = tab.dataset.status;
    load().catch((error) => notify(error.message, true));
  });
  $("#records").addEventListener("click", (event) => {
    const button = event.target.closest("button[data-action]");
    if (button) act(button);
  });
}

init().catch((error) => notify(error.message, true));

let username = "";
let isAdmin = false;
let rooms = [];
let currentRoom = null;
let messageSocket;
let reconnectTimer;
let reconnectDelay = 1000;
let latestMessageId = 0;
const renderedMessageIds = new Set();
let oldestId = null;
let hasMore = false;
let loadingOlder = false;
let loadingMessages = false;
let messageAbortController = null;
let messageQuery = "";
let messageOrder = "asc";
let toastTimer;

const $ = (id) => document.getElementById(id);
const setText = (id, text) => {
  const element = $(id);
  if (element) element.textContent = text;
};
const setError = setText;
const show = (id) => $(id)?.classList.remove("hidden");
const hide = (id) => $(id)?.classList.add("hidden");

function avatarColor(name) {
  let hash = 0;
  for (let i = 0; i < name.length; i++) hash = (hash * 31 + name.charCodeAt(i)) >>> 0;
  return `av-${hash % 8}`;
}

function closeRoomMenu() {
  document.body.classList.remove("room-menu-open");
  $("roomMenuButton")?.setAttribute("aria-expanded", "false");
}

function toggleRoomMenu() {
  const isOpen = document.body.classList.toggle("room-menu-open");
  $("roomMenuButton")?.setAttribute("aria-expanded", String(isOpen));
}

async function api(path, options = {}) {
  const { headers = {}, ...requestOptions } = options;
  const response = await fetch(path, {
    credentials: "same-origin",
    ...requestOptions,
    headers: {
      "Content-Type": "application/json",
      ...headers,
    },
  });
  const data = await response.json().catch(() => ({ error: "服务器错误" }));
  if (!response.ok) throw Error(data.error || "请求失败");
  return data;
}

function openDialog(id) {
  show(id);
  $(id).querySelector("input")?.focus();
}

function closeDialog(id) {
  hide(id);
  setError(id === "registerDialog" ? "registerError" : "roomError", "");
}

function stopConversation() {
  if (reconnectTimer) clearTimeout(reconnectTimer);
  reconnectTimer = null;
  messageSocket?.close();
  messageSocket = null;
  messageAbortController?.abort();
  messageAbortController = null;
  loadingMessages = false;
  loadingOlder = false;
  renderedMessageIds.clear();
}

function scheduleSocketReconnect(roomId) {
  if (reconnectTimer || !currentRoom || currentRoom.id !== roomId) return;
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null;
    connectMessageSocket(roomId);
  }, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 15000);
}

async function syncMessagesAfter(roomId, after) {
  if (after == null || currentRoom?.id !== roomId) return;
  try {
    let cursor = after;
    let hasMoreSync = true;
    while (hasMoreSync && currentRoom?.id === roomId) {
      const data = await api(`/api/messages?room=${encodeURIComponent(roomId)}&after=${encodeURIComponent(cursor)}&limit=100`);
      data.messages.forEach((item) => appendLiveMessage(item, false));
      if (data.messages.length) cursor = Math.max(...data.messages.map((item) => Number(item.id)));
      hasMoreSync = data.has_more;
    }
  } catch (error) {
    if (currentRoom?.id === roomId) alertBox(`消息同步失败：${error.message}`);
  }
}

function appendLiveMessage(item, follow = true) {
  const box = $("messages");
  latestMessageId = Math.max(latestMessageId, Number(item.id));
  if (messageQuery || $("messageDate")?.value) return;
  const messageId = String(item.id);
  if (renderedMessageIds.has(messageId)) return;
  renderedMessageIds.add(messageId);
  if (messageOrder === "desc") box.prepend(messageElement(item));
  else box.append(messageElement(item));
  if (follow) box.scrollTop = messageOrder === "desc" ? 0 : box.scrollHeight;
}

function connectMessageSocket(roomId) {
  if (!currentRoom || currentRoom.id !== roomId) return;
  const protocol = location.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(`${protocol}//${location.host}/api/ws`);
  messageSocket = socket;
  socket.onopen = () => {
    reconnectDelay = 1000;
    socket.send(JSON.stringify({ room: roomId, last_id: latestMessageId }));
    void syncMessagesAfter(roomId, latestMessageId);
  };
  socket.onmessage = (event) => {
    let data;
    try { data = JSON.parse(event.data); } catch { return; }
    if (data.type === "message" && data.message?.room_id === roomId) {
      appendLiveMessage(data.message);
    } else if (data.type === "sync_required") {
      void syncMessagesAfter(roomId, latestMessageId);
    }
  };
  socket.onclose = () => {
    if (messageSocket === socket) {
      messageSocket = null;
      scheduleSocketReconnect(roomId);
    }
  };
  socket.onerror = () => socket.close();
}

async function doLogin(event) {
  event.preventDefault();
  try {
    const data = await api("/api/auth/login", {
      method: "POST",
      body: JSON.stringify({
        username: $("loginUser").value.trim(),
        password: $("loginPass").value,
      }),
    });
    username = data.user.username;
    isAdmin = data.user.is_admin === true;
    await enterApp();
  } catch (error) {
    setError("loginError", error.message);
  }
}

async function doRegister(event) {
  event.preventDefault();
  const user = $("registerUser").value.trim();
  const password = $("registerPass").value;
  const confirmPassword = $("registerConfirm").value;
  if (password !== confirmPassword)
    return setError("registerError", "两次输入的密码不一致");
  try {
    await api("/api/auth/register", {
      method: "POST",
      body: JSON.stringify({
        username: user,
        password,
        password_confirm: confirmPassword,
        invite_code: $("registerInvite").value.trim(),
      }),
    });
    closeDialog("registerDialog");
    $("loginUser").value = user;
    $("loginPass").value = password;
    await doLogin({ preventDefault() {} });
  } catch (error) {
    setError("registerError", error.message);
  }
}

async function enterApp() {
  hide("loginView");
  setText("currentUser", username);
  const accountAvatar = $("accountAvatar");
  accountAvatar.textContent = username.slice(0, 1).toUpperCase();
  accountAvatar.className = `account-avatar ${avatarColor(username)}`;
  if (!(await loadRooms())) return;
  restoreLastRoom();
  show("appView");
}

function restoreLastRoom() {
  const saved = localStorage.getItem("y2m_last_room");
  if (!saved) return;
  const room = rooms.find((item) => String(item.id) === saved);
  if (room && room.joined) {
    void selectRoom(room);
  } else {
    localStorage.removeItem("y2m_last_room");
  }
}

async function loadRooms() {
  try {
    const data = await api("/api/rooms");
    if (!Array.isArray(data.rooms)) throw Error("聊天室数据格式错误");
    rooms = data.rooms;
    renderRooms();
    return true;
  } catch (error) {
    logout();
    setError("loginError", error.message);
    return false;
  }
}

function renderRooms() {
  const query = $("roomSearch").value.trim().toLocaleLowerCase();
  const visible = rooms.filter(
    (room) =>
      !query ||
      String(room.name).toLocaleLowerCase().includes(query) ||
      String(room.code).toLocaleLowerCase().includes(query) ||
      String(room.owner).toLocaleLowerCase().includes(query),
  );
  $("roomList").replaceChildren();
  if (!visible.length) {
    const empty = document.createElement("div");
    empty.className = "no-rooms";
    empty.textContent = rooms.length ? "没有匹配的房间" : "暂无聊天室";
    $("roomList").append(empty);
    return;
  }
  visible.forEach((room) => {
    const item = document.createElement("button");
    item.className = `room-item${currentRoom?.id === room.id ? " selected" : ""}`;
    item.type = "button";
    const icon = document.createElement("span");
    icon.className = `room-icon ${avatarColor(room.name)}`;
    icon.textContent = (room.name || "?").trim().slice(0, 1).toUpperCase();
    const info = document.createElement("span");
    const name = document.createElement("b");
    name.textContent = room.name;
    const meta = document.createElement("small");
    meta.textContent = `${room.code} · ${room.owner}`;
    info.append(name, meta);
    item.append(icon, info);
    if (room.system || room.joined) {
      const state = document.createElement("i");
      state.textContent = room.system ? "公共" : "已加入";
      item.append(state);
    }
    item.onclick = () => (room.joined ? selectRoom(room) : join(room.code));
    $("roomList").append(item);
  });
}

async function join(code) {
  try {
    const room = await api("/api/rooms/join", {
      method: "POST",
      body: JSON.stringify({ code }),
    });
    await loadRooms();
    await selectRoom(rooms.find((item) => item.id === room.id) || room);
    closeRoomMenu();
  } catch (error) {
    alertBox(error.message);
  }
}

async function selectRoom(room) {
  closeRoomMenu();
  stopConversation();
  currentRoom = room;
  localStorage.setItem("y2m_last_room", String(room.id));
  renderedMessageIds.clear();
  oldestId = null;
  latestMessageId = 0;
  hasMore = false;
  messageQuery = "";
  $("messageSearch").value = "";
  $("messageDate") && ($("messageDate").value = "");
  setText("roomName", room.name);
  setText("roomMeta", `房间码：${room.code} · 房主：${room.owner}`);
  hide("noConversation");
  show("conversationView");
  show("messageForm");
  $("messageInput").disabled = false;
  $("sendButton").disabled = false;
  $("copyCode").classList.remove("hidden");
  $("deleteRoom").classList.toggle(
    "hidden",
    room.owner !== username || room.system,
  );
  renderRooms();
  await loadMessages(true);
  if (currentRoom?.id === room.id) connectMessageSocket(room.id);
}

function messageElement(item) {
  const row = document.createElement("div");
  row.className = `message${item.username === username ? " own" : ""}`;
  row.dataset.messageId = item.id;
  const time = new Date(item.created_at * 1000).toLocaleString([], {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
  const avatar = document.createElement("span");
  avatar.className = `message-avatar ${avatarColor(item.username)}`;
  avatar.textContent = item.username.slice(0, 1).toUpperCase();

  const body = document.createElement("div");
  body.className = "message-body";

  const meta = document.createElement("div");
  meta.className = "message-meta";
  const sender = document.createElement("b");
  sender.textContent = item.username;
  const timestamp = document.createElement("span");
  timestamp.className = "message-time";
  timestamp.textContent = time;
  meta.append(sender, timestamp);

  const bubble = document.createElement("div");
  bubble.className = "message-bubble";
  bubble.textContent = item.text;

  body.append(meta, bubble);
  row.append(avatar, body);
  return row;
}

async function loadMessages(reset = false, refresh = false, older = false) {
  if (!currentRoom || loadingMessages || (!older && loadingOlder)) return;
  const roomId = currentRoom.id;
  const box = $("messages");
  const followNewMessages =
    reset &&
    refresh &&
    messageQuery === "" &&
    !$("messageDate")?.value &&
    (messageOrder === "desc"
      ? box.scrollTop < 48
      : box.scrollHeight - box.scrollTop - box.clientHeight < 48);
  const controller = new AbortController();
  messageAbortController = controller;
  loadingMessages = true;
  try {
    const params = new URLSearchParams({ room: roomId, limit: "50" });
    if (messageQuery) params.set("q", messageQuery);
    if ($("messageDate")?.value) {
      const date = new Date(`${$("messageDate").value}T00:00:00`);
      params.set("from", String(Math.floor(date.getTime() / 1000)));
      params.set(
        "to",
        String(Math.floor((date.getTime() + 24 * 60 * 60 * 1000) / 1000)),
      );
    }
    if (!reset && oldestId) params.set("before", oldestId);
    const data = await api(`/api/messages?${params}`, {
      signal: controller.signal,
    });
    if (controller.signal.aborted || currentRoom?.id !== roomId) return;
    if (reset && !refresh) {
      box.replaceChildren();
      renderedMessageIds.clear();
      oldestId = null;
    }
    if (reset && refresh && messageQuery === "" && !$("messageDate")?.value) {
      const existing = new Set(
        [...box.querySelectorAll("[data-message-id]")].map(
          (element) => element.dataset.messageId,
        ),
      );
      const newMessages = data.messages.filter((item) => !existing.has(String(item.id)));
      newMessages.forEach((item) => renderedMessageIds.add(String(item.id)));
      if (messageOrder === "desc") box.prepend(...newMessages.reverse().map(messageElement));
      else box.append(...newMessages.map(messageElement));
      if (newMessages.length && followNewMessages)
        box.scrollTop = messageOrder === "desc" ? 0 : box.scrollHeight;
    } else if (reset) {
      const ordered =
        messageOrder === "desc" ? [...data.messages].reverse() : data.messages;
      renderedMessageIds.clear();
      ordered.forEach((item) => renderedMessageIds.add(String(item.id)));
      box.replaceChildren(...ordered.map(messageElement));
    } else {
      const height = box.scrollHeight;
      const ordered =
        messageOrder === "desc" ? [...data.messages].reverse() : data.messages;
      ordered.forEach((item) => renderedMessageIds.add(String(item.id)));
      if (messageOrder === "desc") box.append(...ordered.map(messageElement));
      else box.prepend(...ordered.map(messageElement));
      if (messageOrder === "asc") box.scrollTop = box.scrollHeight - height;
    }
    if (data.messages.length)
      oldestId = Math.min(
        ...data.messages.map((item) => item.id),
        oldestId || Infinity,
      );
    if (data.messages.length)
      latestMessageId = Math.max(latestMessageId, ...data.messages.map((item) => Number(item.id)));
    hasMore = data.has_more;
    box.classList.toggle("has-more", hasMore);
    if (reset && !refresh)
      box.scrollTop = messageOrder === "desc" ? 0 : box.scrollHeight;
  } catch (error) {
    if (controller.signal.aborted) return;
    if (!refresh) alertBox(error.message);
  } finally {
    if (messageAbortController === controller) {
      loadingMessages = false;
      messageAbortController = null;
    }
  }
}

async function loadOlder() {
  if (!hasMore || loadingOlder) return;
  loadingOlder = true;
  await loadMessages(false, false, true);
  loadingOlder = false;
}

async function queryMessages() {
  messageQuery = $("messageSearch").value.trim();
  messageOrder = $("messageOrder").value;
  await loadMessages(true);
  closeSearch();
}

async function send(event) {
  event.preventDefault();
  const input = $("messageInput");
  const text = input.value.trim();
  if (!text || !currentRoom) return;
  const roomId = currentRoom.id;
  try {
    await api("/api/messages", {
      method: "POST",
      body: JSON.stringify({ room: roomId, text }),
    });
    if (currentRoom?.id === roomId) {
      if (input.value.trim() === text) {
        input.value = "";
        input.style.height = `${parseFloat(getComputedStyle(input).minHeight) || 46}px`;
      }
    }
  } catch (error) {
    alertBox(error.message);
  }
}

async function createRoom(event) {
  event.preventDefault();
  try {
    const data = await api("/api/rooms", {
      method: "POST",
      body: JSON.stringify({ name: $("newRoomName").value.trim() }),
    });
    closeDialog("roomDialog");
    await loadRooms();
    await selectRoom(rooms.find((room) => room.id === data.id) || data);
    closeRoomMenu();
  } catch (error) {
    setError("roomError", error.message);
  }
}

async function removeRoom() {
  if (!currentRoom || !confirm(`确定删除聊天室“${currentRoom.name}”吗？`))
    return;
  try {
    await api(`/api/rooms/${currentRoom.id}`, { method: "DELETE" });
    localStorage.removeItem("y2m_last_room");
    stopConversation();
    currentRoom = null;
    show("noConversation");
    hide("conversationView");
    hide("messageForm");
    setText("roomName", "请选择聊天室");
    setText("roomMeta", "从左侧选择一个公共房间");
    $("messages").replaceChildren();
    $("messageInput").disabled = true;
    $("sendButton").disabled = true;
    hide("copyCode");
    hide("deleteRoom");
    await loadRooms();
  } catch (error) {
    alertBox(error.message);
  }
}

function copyRoomCode() {
  if (!currentRoom) return;
  if (!navigator.clipboard) {
    alertBox("当前浏览器不支持复制");
    return;
  }
  navigator.clipboard
    .writeText(currentRoom.code)
    .then(() => alertBox("房间码已复制"))
    .catch(() => alertBox("复制房间码失败"));
}

function closeSearch() {
  document.body.classList.remove("search-open");
}

function openSettings() {
  let dialog = document.getElementById("settingsDialog");
  if (dialog && dialog.dataset.adminPanel !== (isAdmin ? "1" : "0")) {
    dialog.remove();
    dialog = null;
  }
  if (!dialog) {
    dialog = document.createElement("div");
    dialog.id = "settingsDialog";
    dialog.className = "modal-backdrop";
    dialog.dataset.adminPanel = isAdmin ? "1" : "0";

    const tabs = isAdmin
      ? `<div class="settings-tabs">
           <button class="settings-tab active" type="button" data-tab="account">账户</button>
           <button class="settings-tab" type="button" data-tab="server">服务器</button>
           <button class="settings-tab" type="button" data-tab="database">数据库</button>
         </div>`
      : "";

    const serverPanel = isAdmin
      ? `<div class="settings-panel hidden" data-panel="server">
           <p class="kicker">SERVER ACCESS</p>
           <h3>注册设置</h3>
           <form id="adminSettingsForm">
             <label>注册模式<select id="registrationMode"><option value="open">开放注册</option><option value="invite">仅限邀请码</option></select></label>
             <label>邀请码<input id="adminInviteCode" maxlength="128" placeholder="设置邀请码"></label>
             <div id="adminSettingsError" class="form-error"></div>
             <button class="button" type="submit">保存注册设置</button>
           </form>
         </div>`
      : "";

    const databasePanel = isAdmin
      ? `<div class="settings-panel hidden" data-panel="database">
           <p class="kicker">DATABASE</p>
           <h3>数据备份</h3>
           <p class="modal-copy">下载数据库快照，或将备份文件恢复以覆盖当前数据。恢复后服务会自动重启。</p>
           <div class="backup-actions">
             <button id="downloadBackup" class="button" type="button">下载备份</button>
             <button id="restoreBackup" class="soft-button" type="button">恢复备份</button>
           </div>
           <input id="restoreFile" type="file" accept=".sqlite3,.db,.sqlite,application/octet-stream" class="hidden">
         </div>`
      : "";

    dialog.innerHTML = `<div class="modal settings-modal">
        <button class="modal-close" type="button" data-close>×</button>
        <h2>设置</h2>
        ${tabs}
        <div class="settings-panel" data-panel="account">
          <p class="kicker">ACCOUNT SECURITY</p>
          <p class="modal-copy">用户名 <strong>${escapeHtml(username)}</strong> 注册后不可修改。</p>
          <form id="passwordForm">
            <label>当前密码<input id="settingsCurrentPassword" type="password" required></label>
            <label>新密码<input id="settingsNewPassword" type="password" minlength="6" required></label>
            <label>确认新密码<input id="settingsConfirmPassword" type="password" minlength="6" required></label>
            <div id="passwordError" class="form-error"></div>
            <button class="button button-dark" type="submit">保存新密码 <span>↗</span></button>
          </form>
        </div>
        ${serverPanel}
        ${databasePanel}
      </div>`;
    document.body.append(dialog);

    dialog.querySelector("[data-close]").onclick = () => hide("settingsDialog");
    dialog.querySelector("#passwordForm").onsubmit = savePassword;
    dialog.querySelector("#adminSettingsForm")?.addEventListener("submit", saveAdminSettings);

    if (isAdmin) {
      dialog.querySelectorAll(".settings-tab").forEach((tab) => {
        tab.onclick = () => switchSettingsTab(dialog, tab.dataset.tab);
      });
      dialog.querySelector("#registrationMode").onchange = () =>
        setError("adminSettingsError", "");
      dialog.querySelector("#adminInviteCode").oninput = () =>
        setError("adminSettingsError", "");
      dialog.querySelector("#downloadBackup").onclick = downloadBackup;
      dialog.querySelector("#restoreBackup").onclick = () =>
        dialog.querySelector("#restoreFile").click();
      dialog.querySelector("#restoreFile").onchange = onRestoreFile;
    }
  }
  show("settingsDialog");
  switchSettingsTab(dialog, "account");
  dialog.querySelector("#settingsCurrentPassword").focus();
  if (isAdmin) void loadAdminSettings();
}

function switchSettingsTab(dialog, name) {
  dialog.querySelectorAll(".settings-tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.tab === name);
  });
  dialog.querySelectorAll(".settings-panel").forEach((panel) => {
    panel.classList.toggle("hidden", panel.dataset.panel !== name);
  });
}

async function downloadBackup() {
  try {
    const response = await fetch("/api/admin/backup", {
      credentials: "same-origin",
    });
    if (!response.ok) {
      const data = await response.json().catch(() => ({}));
      throw Error(data.error || "下载备份失败");
    }
    const blob = await response.blob();
    const disposition = response.headers.get("Content-Disposition") || "";
    const match = /filename="?([^";]+)"?/.exec(disposition);
    const filename = match ? match[1] : "y2m-backup.sqlite3";
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = filename;
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
    URL.revokeObjectURL(url);
  } catch (error) {
    alertBox(error.message);
  }
}

function onRestoreFile(event) {
  const file = event.target.files && event.target.files[0];
  event.target.value = "";
  if (!file) return;
  if (!confirm("此操作会用备份覆盖当前所有数据，服务将自动重启，确定继续吗？")) return;
  void restoreBackup(file);
}

async function restoreBackup(file) {
  try {
    const response = await fetch("/api/admin/backup/restore", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/octet-stream" },
      body: file,
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw Error(data.error || "恢复失败");
    hide("settingsDialog");
    alertBox("恢复成功，服务正在重启，请稍后刷新页面");
  } catch (error) {
    alertBox(error.message);
  }
}

async function loadAdminSettings() {
  try {
    const data = await api("/api/admin/settings");
    $("registrationMode").value = data.registration_mode;
    $("adminInviteCode").value = data.invite_code || "";
    setError("adminSettingsError", "");
  } catch (error) {
    setError("adminSettingsError", error.message);
  }
}

async function saveAdminSettings(event) {
  event.preventDefault();
  try {
    await api("/api/admin/settings", {
      method: "POST",
      body: JSON.stringify({
        registration_mode: $("registrationMode").value,
        invite_code: $("adminInviteCode").value.trim(),
      }),
    });
    alertBox("注册设置已保存");
  } catch (error) {
    setError("adminSettingsError", error.message);
  }
}

async function savePassword(event) {
  event.preventDefault();
  try {
    await api("/api/profile/password", {
      method: "POST",
      body: JSON.stringify({
        current_password: $("settingsCurrentPassword").value,
        new_password: $("settingsNewPassword").value,
        confirm_password: $("settingsConfirmPassword").value,
      }),
    });
    event.target.reset();
    hide("settingsDialog");
    logout();
    setError("loginError", "密码已更新，请重新登录");
  } catch (error) {
    setError("passwordError", error.message);
  }
}

function alertBox(text) {
  const toast = $("toast");
  if (!toast) return;
  clearTimeout(toastTimer);
  toast.textContent = text;
  toast.classList.add("visible");
  toastTimer = setTimeout(() => toast.classList.remove("visible"), 3000);
}

function logout() {
  void api("/api/auth/logout", { method: "POST" }).catch(() => {});
  localStorage.removeItem("y2m_last_room");
  stopConversation();
  isAdmin = false;
  username = "";
  currentRoom = null;
  hide("appView");
  show("loginView");
  hide("messageForm");
  $("loginPass").value = "";
}

function escapeHtml(text) {
  const node = document.createElement("div");
  node.textContent = text;
  return node.innerHTML;
}

$("loginForm").onsubmit = doLogin;
$("registerForm").onsubmit = doRegister;
$("showRegister").onclick = () => openDialog("registerDialog");
$("cancelRegister").onclick = () => closeDialog("registerDialog");
$("newRoom").onclick = () => openDialog("roomDialog");
$("cancelRoom").onclick = () => closeDialog("roomDialog");
$("roomForm").onsubmit = createRoom;
$("refreshRooms").onclick = loadRooms;
$("roomMenuButton").onclick = toggleRoomMenu;
$("emptyRoomMenuButton").onclick = toggleRoomMenu;
$("closeRoomMenu").onclick = closeRoomMenu;
$("roomMenuBackdrop").onclick = closeRoomMenu;
$("roomSearch").oninput = renderRooms;
$("messageSearch").onkeydown = (event) => {
  if (event.key === "Enter") queryMessages();
};
$("searchMessages").onclick = queryMessages;
$("clearMessageSearch").onclick = () => {
  $("messageSearch").value = "";
  if ($("messageDate")) $("messageDate").value = "";
  messageQuery = "";
  closeSearch();
  loadMessages(true);
};
$("messageOrder").onchange = queryMessages;
$("messages").onscroll = (event) => {
  const box = event.target;
  // asc：更早的消息插在顶部，滚到顶部时加载；desc：追加在底部，滚到底部时加载。
  const nearEdge =
    messageOrder === "desc"
      ? box.scrollHeight - box.scrollTop - box.clientHeight < 30
      : box.scrollTop < 30;
  if (nearEdge) loadOlder();
};
$("messageForm").onsubmit = send;
$("messageInput").onkeydown = (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    $("messageForm").requestSubmit();
  }
};
$("messageInput").oninput = (event) => {
  const input = event.target;
  const baseHeight = parseFloat(getComputedStyle(input).minHeight) || 46;
  input.style.height = `${baseHeight}px`;
  if (input.scrollHeight > baseHeight)
    input.style.height = `${Math.min(input.scrollHeight, 120)}px`;
};
$("deleteRoom").onclick = removeRoom;
$("copyCode").onclick = copyRoomCode;
$("logout").onclick = logout;
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeRoomMenu();
});
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && currentRoom && messageSocket?.readyState === WebSocket.OPEN)
    void syncMessagesAfter(currentRoom.id, latestMessageId);
});
const settingsButton = document.createElement("button");
settingsButton.type = "button";
settingsButton.className = "drawer-setting";
settingsButton.textContent = "设置";
settingsButton.onclick = openSettings;
$("logout").before(settingsButton);
document.querySelector(".search-toggle").onclick = () => {
  document.body.classList.add("search-open");
  $("messageSearch").focus();
};
document.querySelector(".message-query .search-close").onclick = closeSearch;
hide("copyCode");

api("/api/auth/me")
  .then((data) => {
    username = data.user.username;
    isAdmin = data.user.is_admin === true;
    return enterApp();
  })
  .catch(() => show("loginView"));

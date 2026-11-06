"use strict";

const api = window.cacaduu;
const $ = (id) => document.getElementById(id);
let current = null;

function time(ms, never) {
  if (!ms) return never;
  return new Date(ms).toLocaleString(undefined, { day: "numeric", month: "numeric", hour: "2-digit", minute: "2-digit" });
}

function render(s) {
  current = s;
  const t = s.strings;
  document.documentElement.lang = s.lang;
  $("title").textContent = t.title;
  $("intro").textContent = t.intro;

  // sign-in
  $("login").hidden = s.signedIn;
  $("sign_in_title").textContent = t.sign_in_title;
  $("l_email").textContent = t.email;
  $("l_password").textContent = t.password;
  if (!$("sign_in").disabled) $("sign_in").textContent = t.sign_in;
  if (!s.signedIn && s.email && !$("email").value) $("email").value = s.email;

  // the export folder
  $("sync").hidden = !s.signedIn;
  $("l_shop").textContent = t.shop;
  $("shop_row").hidden = s.shops.length < 2; // one shop is chosen by itself
  const select = $("shop");
  const options = [{ id: "", name: "—" }, ...s.shops];
  if (select.dataset.key !== JSON.stringify(options)) {
    select.replaceChildren(...options.map((shop) => new Option(shop.name, shop.id)));
    select.dataset.key = JSON.stringify(options);
  }
  select.value = s.shopId || "";

  $("l_folder").textContent = t.folder;
  $("folder_path").textContent = s.folder || t.no_folder;
  $("folder_path").classList.toggle("empty", !s.folder);
  $("choose_folder").textContent = s.folder ? t.change_folder : t.choose_folder;

  $("status").textContent = s.checking ? t.checking : s.statusText;
  const tone =
    s.phase === "error" || s.phase === "signed_out"
      ? "bad"
      : s.phase === "other_source" || s.phase === "setup" || /^(settling|result_proposed|result_waiting_for_approval|result_layout_changed|no_file)$/.test(s.message.key)
        ? "warn"
        : s.phase === "watching"
          ? "ok"
          : "";
  $("dot").className = `dot ${tone}`;

  $("l_latest").textContent = t.latest_file;
  $("latest").textContent = s.file ? `${s.file.name} (${time(s.file.mtimeMs, t.never)})` : t.none;
  $("l_last").textContent = t.last_check;
  $("last").textContent = time(s.lastCheck, t.never);
  $("l_next").textContent = t.next_check;
  $("next").textContent = s.checking ? t.checking : time(s.nextCheck, t.none);

  $("check_now").textContent = s.checking ? t.checking : t.check_now;
  $("check_now").disabled = s.checking;
  $("open_site").textContent = t.open_site;

  $("autostart_row").hidden = !s.canAutostart;
  $("autostart").checked = s.autostart;
  $("l_autostart").textContent = t.autostart;
  $("who").textContent = s.email ? s.email : "";
  $("sign_out").textContent = t.sign_out;
}

$("login").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = $("sign_in");
  button.disabled = true;
  button.textContent = current.strings.signing_in;
  $("login_error").hidden = true;
  const answer = await api.signIn($("email").value, $("password").value);
  button.disabled = false;
  button.textContent = current.strings.sign_in;
  if (!answer.ok) {
    $("login_error").textContent = answer.error;
    $("login_error").hidden = false;
    return;
  }
  $("password").value = "";
  render(await api.state());
});

$("shop").addEventListener("change", async (event) => render(await api.chooseShop(event.target.value)));
$("choose_folder").addEventListener("click", async () => render(await api.chooseFolder()));
$("check_now").addEventListener("click", async () => render(await api.checkNow()));
$("open_site").addEventListener("click", () => api.openSite());
$("autostart").addEventListener("change", async (event) => render(await api.setAutostart(event.target.checked)));
$("sign_out").addEventListener("click", async (event) => {
  event.preventDefault();
  render(await api.signOut());
});

api.onState(render);
api.state().then(render);

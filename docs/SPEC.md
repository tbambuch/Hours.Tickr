# TimeWatch – specifikace

Malá desktopová aplikace (Windows + macOS), která měří čas práce na PC a synchronizuje ho do **My Hours** (https://app.myhours.com).

UI texty jsou **česky**. Kód, komentáře, názvy souborů anglicky.

---

## 1. Technologie a struktura

- **Tauri 2** (Rust) a webview s prostým HTML/CSS/JS (bez bundleru a frameworku). Logika je v Rustu, UI ve složce `ui/`.
- Čistá logika je v samostatném crate `timewatch-core` (bez Tauri a OS API), takže se testuje kdekoli (`cargo test`).
- Balení: `tauri build` (Windows: NSIS installer, macOS: dmg; dmg se staví na macOS, např. v CI).
- Bezpečnost: okna smějí volat jen příkazy povolené v `src-tauri/capabilities/`, striktní CSP v `tauri.conf.json`. Přihlašovací okno (vzdálený obsah) k příkazům přístup nemá.
- Single-instance (`tauri-plugin-single-instance`) – druhé spuštění jen zobrazí okno.

Struktura:

```
src-tauri/core/src/tracker.rs       – stavový automat měření (čistá logika, testovatelná)
src-tauri/core/src/sync_queue.rs    – perzistentní fronta operací vůči API + retry
src-tauri/core/src/api.rs           – tenký HTTP klient, injektovatelný pro testy
src-tauri/core/src/auth.rs          – OAuth (PKCE) a správa tokenů
src-tauri/core/src/secrets.rs       – přihlašovací údaje v systémovém úložišti (keyring)
src-tauri/core/src/store.rs         – perzistence stavu (JSON v adresáři dat aplikace), atomický zápis
src-tauri/core/src/power.rs         – lock/unlock, suspend/resume, shutdown → volání trackeru
src-tauri/core/src/idle_watcher.rs  – pozastavení při nečinnosti
src-tauri/core/src/i18n.rs          – slovníky textů (locales/*.json)
src-tauri/src/                      – aplikace: okna, tray, IPC příkazy, platformní kód (Windows, macOS)
ui/timer.html|css|js                – hlavní mini okno
ui/settings.html|css|js             – nastavení
assets/                             – ikony (tray idle / recording, .ico/.icns)
```

## 2. My Hours API (ověřeno z oficiální dokumentace v1.1)

Dokumentace: https://documenter.getpostman.com/view/8879268/TVmV4YYU

- Base URL: `https://api2.myhours.com/api`
- Autentizace: API klíč, hlavička `Authorization: ApiKey <KEY>`. Přidej i `Accept: application/json`, `api-version: 1.0`, u POST `Content-Type: application/json`.
  - Uživatel si klíč vytvoří v My Hours (nastavení profilu → API key). Aplikace nepracuje s heslem.
- Všechny časy v **ISO8601 UTC** (`new Date().toISOString()`).

Použité endpointy:

| Účel | Metoda + cesta | Tělo / parametry |
|---|---|---|
| Aktivní projekty | `GET /Projects` | – (vrací pole projektů, `id`, `name`, …) |
| Úkoly projektu | `GET /Projects/{projectId}/tasklist` | – (struktura není zdokumentovaná – parsuj defenzivně: může to být pole tasků, nebo pole task-listů s vnořenými `tasks`/`incompletedTasks`; hledej objekty s `id` + `name`) |
| Poslední logy | `GET /Logs?date=YYYY-MM-DD&step=100` | `date`, `step` povinné, `startIndex` volitelný |
| Start timeru | `POST /Logs/startNewLog` | `{ projectId, taskId, note, date, start, billable? }` → odpověď obsahuje ID nového logu (pravděpodobně `id`; parsuj defenzivně, např. `res.id ?? res.logId ?? res`) |
| Stop timeru | `POST /Logs/stoptimer` | `{ logId, time }` |
| Vložit hotový log | `POST /Logs/insertlog` | `{ projectId, taskId, note, date, start, end }` (nebo `duration` v sekundách místo `start/end`) |

Poznámky:
- `projectId` je povinný; uživatel musí být k projektu přiřazen ve webové aplikaci. `taskId` může být `null`, pokud uživatel úkol nezvolí.
- `date` u `startNewLog` a `insertlog`: pošli **lokální datum** začátku segmentu ve formátu `YYYY-MM-DD` (dokumentace má v příkladu i plný ISO timestamp – zkus nejdřív `YYYY-MM-DD`; když API vrátí 400, je to místo k úpravě, nech to v jedné funkci).
- Chybové odpovědi: 401/403 = neplatný klíč → zobraz v UI „Neplatný API klíč“, nepokoušej se o retry dokud se klíč nezmění. 4xx (kromě 408/429) = trvalá chyba operace → zaloguj, označ operaci jako failed, zobraz varování. 5xx / síťová chyba / 408 / 429 = dočasné → retry.

## 3. Model měření

Měření se skládá ze **segmentů**. Segment = souvislý úsek běhu (od startu/obnovení do stopu/pauzy). Každý segment = **jeden time log v My Hours**.

Stavy trackeru:
- `stopped` – uživatel měření vypnul (nebo nikdy nezapnul).
- `running` – běží segment.
- `paused` – měření je **zapnuté uživatelem**, ale dočasně přerušené systémem (lockscreen, uspání, vypnutí PC). Po odstranění důvodu se automaticky obnoví.

Klíčové perzistentní pole: `userWantsRunning` (boolean). Nastavuje ho **jen uživatel** (tlačítko / tray). Systémové události mění jen `running ↔ paused`.

Lokální stav (`state.json` v userData), zapisuj **atomicky** (zápis do `.tmp` + `rename`) a **synchronně** (`fs.writeFileSync`) při každé změně – kvůli vypnutí PC:

```json
{
  "userWantsRunning": true,
  "current": { "localId": "uuid", "start": "ISO", "remoteLogId": 123 | null },
  "lastHeartbeat": "ISO",
  "pauseReason": null | "lock" | "suspend" | "shutdown",
  "segments": [ { "localId", "start", "end", "remoteLogId", "syncState": "pending|synced|failed" } ],
  "queue": [ ...operace syncQueue... ]
}
```

Segmenty starší než 7 dní z lokálního stavu mazat (jen ty `synced`).

### Události → akce

| Událost | Akce |
|---|---|
| Klik Start (stav `stopped`) | `userWantsRunning = true`, otevři segment (start = teď) |
| Klik Stop | `userWantsRunning = false`, uzavři segment (end = teď), **okamžitě sync** |
| zámek obrazovky | pokud `running` → uzavři segment (end = teď), stav `paused`, reason `lock`, sync |
| odemknutí obrazovky | pokud `paused` a `userWantsRunning` → nový segment (start = teď) |
| uspání (suspend) | jako lock, reason `suspend` |
| probuzení (resume) | **neobnovuj hned**, pokud je obrazovka zamčená (po probuzení typicky lockscreen). Obnov, až přijde odemknutí obrazovky. Pokud do ~5 s nepřijde žádný lock a systém nehlásí zamčenou obrazovku, obnov. |
| vypnutí / odhlášení systému (Windows `WM_QUERYENDSESSION` / `WM_ENDSESSION`, macOS ukončení aplikace) | synchronně uzavři segment (end = teď) a zapiš stav na disk, reason `shutdown`, zkus odeslat stop (best effort, nečekej) |
| `app` `before-quit` (uživatel dal „Ukončit“) | uzavři segment, **neměň** `userWantsRunning` (při dalším startu PC se má měření obnovit), počkej max ~3 s na sync a pak ukonči |
| Start aplikace | 1) **Obnova po pádu**: pokud `current` existuje (segment nebyl uzavřen – pád, výpadek proudu, tvrdé vypnutí), uzavři ho časem `lastHeartbeat`. 2) Pokud `userWantsRunning` → otevři nový segment (start = teď). 3) Spusť sync. |
| Heartbeat každých 30 s | `lastHeartbeat = teď` (zapsat na disk) |
| Lokální půlnoc během běhu | uzavři segment v 23:59:59.999 a otevři nový v 00:00 (logy se počítají po dnech) |
| Změna projektu/úkolu/poznámky v nastavení během běhu | uzavři segment, otevři nový s novými hodnotami |

Na Windows je kvůli `WM_ENDSESSION` potřeba skryté okno nejvyšší úrovně, které naslouchá zprávám systému (message-only okno je nedostává).

Pozor na zdvojené události (lock + suspend zároveň, opakovaný unlock): automat musí být idempotentní.

## 4. Synchronizace (syncQueue)

Cíl: v My Hours vždy co nejdřív odpovídající data, i offline.

Operace ve frontě (perzistentní, zpracovávané **sekvenčně** v pořadí):
- `start(localId, start, projectId, taskId, note)` → `startNewLog`, uloží `remoteLogId` do segmentu.
- `stop(localId, end)` → `stoptimer { logId: remoteLogId, time: end }`.
- Kolaps: pokud se `stop` zpracovává a segment ještě nemá `remoteLogId` (start se neodeslal, např. offline) → nahraď dvojici start+stop jedním `insertlog` se `start/end`. Totéž při obnově po pádu.
- Segmenty kratší než 60 s neposílej vůbec (zahoď start i stop, pokud start ještě neodešel; pokud už odešel, normálně stop).

Spouštění:
- okamžitě po každé změně stavu,
- periodicky každých **5 minut** (konfigurovatelné v nastavení),
- při startu aplikace,
- retry dočasných chyb s exponenciálním backoffem (30 s → max 5 min).

Periodická kontrola při běžícím segmentu: zavolej `GET /Logs?date=<dnes>&step=100` a ověř, že log s `remoteLogId` existuje. Pokud ne (smazán na webu), zaloguj a založ ho znovu (`startNewLog` s původním `start`). Chyby této kontroly nesmí měření rozbít.

Stav synchronizace vystav do UI: `ok` / `pending` (počet čekajících operací) / `offline` / `auth-error` / `error`.

## 5. Hlavní okno (mini timer)

- Bezrámové (`frame: false`), neprůhledné, nezměnitelná velikost, cca **180 × 190 px**, `skipTaskbar: true`, zaoblené rohy, tmavý i světlý motiv podle systému (`prefers-color-scheme`).
- Výchozí pozice: **pravý dolní roh** pracovní plochy primárního displeje (`screen.getPrimaryDisplay().workArea`), odsazení 12 px.
- Přesouvání tažením (`-webkit-app-region: drag` na pozadí; tlačítka `no-drag`). Pozici ukládej (debounce) a při startu obnov; pokud uložená pozice není na žádném aktuálním displeji, použij výchozí.
- Obsah:
  - Velké kulaté tlačítko **Record**: ve stavu `stopped` červené plné kolečko; ve stavu `running` / `paused` ikona **Stop** (zaoblený čtverec). Ve stavu `paused` tlačítko lehce ztlumené a pod ním text „Pozastaveno“.
  - Pod tlačítkem **dnešní naměřený čas** ve formátu `H:MM` (např. `3:07`), velké číslo. Počítá se lokálně jako součet dnešních segmentů + běžící segment; aktualizace každou sekundu, ale zobrazení jen h:mm.
  - Malý řádek se stavem sync (ikona/tečka + tooltip s detailem) a názvem projektu (zkrácený).
  - V rohu malá tlačítka: **📌 pin** (always on top; toggle, stav vizuálně zřetelný, persistentní), **⚙** nastavení, **–** skrýt do tray.
- Zavření/skrytí okno jen schová (aplikace běží dál v tray).
- Klávesové ovládání: mezerník/Enter = Start/Stop, s viditelným focus stylem.

## 6. Tray

- Ikona v oznamovací oblasti (Windows) / menu baru (macOS – použij template image). Dvě varianty: idle (šedá) a recording (s červenou tečkou); `paused` = idle varianta. Tooltip: `TimeWatch – 3:07 dnes` + stav.
- Klik (Windows) = zobrazit/skrýt okno.
- Kontextové menu: `Spustit` / `Zastavit`, `Zobrazit okno`, `Vždy navrchu` (checkbox), `Nastavení…`, `Synchronizovat teď`, `Spouštět po startu systému` (checkbox), separator, `Ukončit`.

## 7. Nastavení (samostatné malé okno)

- **API klíč** (input type=password, tlačítko „Ověřit“ → zavolá `GET /Projects`; výsledek zobraz). Klíč ulož do systémového úložiště přihlašovacích údajů (Windows Credential Manager / macOS Keychain); nikdy ho neposílej do rendereru zpět v plném znění (zobraz jen `••••1234`).
- **Projekt** (select naplněný z `GET /Projects`), **Úkol** (select z tasklistu, volba „— bez úkolu —“), **Poznámka** k logům (výchozí prázdná).
- Interval synchronizace (minuty, 1–60, výchozí 5).
- Spouštět po startu systému (výchozí **zapnuto**; `app.setLoginItemSettings({ openAtLogin, openAsHidden: false })`). Při prvním spuštění nastav zapnuto.
- Dokud není vyplněný klíč + projekt, hlavní okno místo času ukáže výzvu „Nastavte My Hours“ a klik na Record otevře nastavení. Měření lze i tak spustit lokálně? **Ne** – bez projektu nespouštět.

## 8. Logování

- Jednoduchý log do `userData/logs/timewatch.log` (rotace při > 1 MB, ponech 1 starý). Loguj události (lock/unlock/suspend/…), sync operace a chyby. **Nikdy neloguj API klíč** ani hlavičku Authorization.

## 9. Testy (povinné)

Unit testy (`cargo test`) pro `tracker` a `sync_queue` s falešnými hodinami a mock API:
- start → lock → unlock → stop = 2 segmenty, `userWantsRunning` false.
- suspend + lock zároveň, resume, unlock → přesně jeden nový segment.
- simulovaný pád (current bez end) → po startu uzavřen časem `lastHeartbeat` a otevřen nový, pokud `userWantsRunning`.
- shutdown → restart aplikace → měření pokračuje novým segmentem.
- offline: start + stop bez sítě → po obnovení sítě jeden `insertlog`.
- segment < 60 s se neodešle.
- 401 → stav `auth-error`, žádné další pokusy.
- přechod přes půlnoc → 2 segmenty.
- výpočet dnešního součtu.

## 10. Skripty

- `npm start` – spustí aplikaci.
- `npm test` – testy.
- `npm run dist:win` – NSIS installer, `npm run dist:mac` – dmg (nastavení připrav).
- README.md (česky): instalace, jak získat API klíč v My Hours, chování při zamčení/vypnutí, kde jsou logy a stav.

## 11. Přihlášení přes účet My Hours (OAuth) – hlavní způsob

Důvod: API klíč je jen v placených tarifech. Webová aplikace My Hours (i na free tarifu) se přihlašuje přes OpenID Connect, Authorization Code + PKCE, a volá stejné API `api2.myhours.com` s Bearer tokenem. TimeWatch udělá totéž. **Neoficiální cesta** (používá client_id webové aplikace) – v README to uveď.

Ověřené parametry (z `https://login.myhours.com/.well-known/openid-configuration` a z přesměrování `app.myhours.com`):

- Authorize: `https://login.myhours.com/connect/authorize`
  - `response_type=code`, `client_id=pkce_client`, `redirect_uri=https://app.myhours.com`, `scope=openid profile api offline_access`, `code_challenge=<S256>`, `code_challenge_method=S256`, `state=<random>`, `nonce=<random>`.
  - Web posílá navíc `AppId=F45FF764-794C-4652-972E-1287F90C7B81` – přidej jako konstantu (účel neznámý, neškodí).
- Token: `POST https://login.myhours.com/connect/token`, `application/x-www-form-urlencoded`
  - výměna kódu: `grant_type=authorization_code&code&redirect_uri=https://app.myhours.com&client_id=pkce_client&code_verifier`
  - obnova: `grant_type=refresh_token&refresh_token&client_id=pkce_client`
  - odpověď: `access_token`, `expires_in`, `refresh_token` (může rotovat – vždy ulož nový), `id_token`.
- Revocation (při odhlášení, best effort): `POST https://login.myhours.com/connect/revocation` s `token`, `token_type_hint=refresh_token`, `client_id=pkce_client`.
- API volání: `Authorization: Bearer <access_token>` (ostatní hlavičky beze změny).

### Login okno
- Samostatné okno cca 480 × 680, titulek „Přihlášení k My Hours“, vlastní perzistentní profil webview (cookies přihlášení zůstanou → příští přihlášení bývá jedním klikem), **bez přístupu k příkazům aplikace**.
- User-Agent okna: výchozí UA webview (Google jinak hlásí „nezabezpečený prohlížeč“). Pokud Google přesto odmítne, uživatel se přihlásí e-mailem a heslem – v README.
- Povol navigaci jen na `https:`. `window.open` → otevřít ve stejném okně (`action: 'deny'` + `loadURL`), ne nové okno.
- Zachycení callbacku: `session.webRequest.onBeforeRequest` pro `https://app.myhours.com/*` (+ záloha `will-redirect`/`will-navigate`) → request zruš, z URL vezmi `code` a `state`, ověř `state`, okno zavři, proveď výměnu kódu. `error` v callbacku → chyba do UI.
- Zavření okna uživatelem = zrušeno (bez chyby, jen stav „Nepřihlášeno“).
- Na stránku My Hours nikdy nic nevpisuj ani nečti DOM – uživatel se přihlašuje sám.

### Tokeny
- Refresh token ulož do systémového úložiště přihlašovacích údajů jako objekt `{ method: 'oauth' | 'apikey', refreshToken?, apiKey? }`.
- Access token jen v paměti; obnov ho, když do vypršení zbývá < 60 s, a při 401 jednou obnov + zopakuj požadavek. Obnova je single-flight.
- `invalid_grant` / 400 z token endpointu = přihlášení vypršelo → stav `auth-error`, UI „Přihlášení vypršelo – přihlaste se znovu“. Síťová chyba / 5xx z token endpointu = dočasná (měření a fronta jedou dál, retry).
- Po úspěšném novém přihlášení zavolej `queue.resetAuth()` a případně `tracker.systemResume('configured')` (jako u klíče).
- Jméno/e-mail přihlášeného uživatele vezmi z `id_token` (dekódovat payload bez ověření podpisu – jen pro zobrazení) a ulož do nastavení (ne tajné).
- Nikdy neloguj tokeny, `code`, `code_verifier`, ani celé URL callbacku.

### UI nastavení
- Nahoře sekce **Účet My Hours**: když nepřihlášen → primární tlačítko „Přihlásit se k My Hours“; když přihlášen → „Přihlášen: Jméno (email)“ + tlačítko „Odhlásit“ (revokace, smazání tokenu, vymazání úložiště partition `persist:myhours-login`, zastavení měření jako u smazání klíče).
- Pod tím sbalitelná volba „Mám placený tarif – použít API klíč“ (stávající UI pro klíč). Aktivní je vždy jen jedna metoda; uložení jedné smaže druhou.
- Projekty a úkoly se načítají stejně, jen přes aktuální metodu.

### Testy (navíc)
- PKCE: verifier 43–128 znaků, challenge = base64url(SHA-256(verifier)).
- Sestavení authorize URL (všechny parametry).
- Parsování callbacku: úspěch, chybný `state`, `error=access_denied`.
- Výměna kódu a obnova tokenu s mock fetch (form-encoded tělo, rotace refresh tokenu).
- API: proaktivní obnova před vypršením; 401 → obnova → opakování úspěšné; `invalid_grant` → auth-error; souběžné požadavky = jediná obnova.
- Tokeny se neobjeví v logu.

### Proč ne systémový prohlížeč (ověřeno 2026-11-06)
- `login.myhours.com` přijímá pro `pkce_client` jen `redirect_uri=https://app.myhours.com` (loopback `http://127.0.0.1:…`, `http://localhost` i jiné cesty končí na `/home/error`).
- Device flow (`/connect/deviceauthorization`) vrací `unauthorized_client`.
- Ve výchozím prohlížeči by tedy kód spotřebovala webová aplikace. Zůstává vestavěné login okno s perzistentní partition.

## 12. Jazyk, téma, tlačítka

### Jazyk
- Podporované jazyky: **čeština (`cs`)** a **angličtina (`en`)**. V nastavení volba „Jazyk“: `Podle systému` (výchozí) / `Čeština` / `English`.
- „Podle systému“: první položka z `app.getPreferredSystemLanguages()` (fallback `app.getLocale()`); `cs*` nebo `sk*` → `cs`, jinak `en`.
- Všechny texty UI ve slovnících `locales/cs.json` a `locales/en.json` (stejné klíče; test hlídá, že obě sady mají shodné klíče a žádná hodnota není prázdná). Patří sem: hlavní okno, nastavení, tray menu a tooltip, titulky oken (včetně login okna), chybové hlášky z aplikace (`user_message`, stavy synchronizace, auth chyby).
- Renderer je sandboxovaný → slovník pro aktuální jazyk mu posílej přes IPC (`i18n:get` + událost při změně). `document.documentElement.lang` nastav podle jazyka.
- Změna jazyka se projeví **okamžitě** bez restartu (obě okna se přerenderují, tray menu a tooltip se přestaví, titulky oken se změní).
- Logy zůstávají anglicky.

### Téma
- V nastavení volba „Vzhled“: `Podle systému` (výchozí) / `Světlý` / `Tmavý`.
- Implementace přes motiv oken (`system` | `light` | `dark`) – CSS dál používá jen `prefers-color-scheme`, takže se přepnou všechna okna. Aplikuj hned po načtení nastavení (před vytvořením oken) a okamžitě při změně.
- Pozadí oken nastav podle aktuálního motivu (aby při otevření neproblikla špatná barva).

### Tlačítko Start/Stop (hlavní okno)
- `stopped`: **zelená šipka „play“** (trojúhelník) na kulatém tlačítku. Popisek pro čtečky „Spustit měření“.
- `running`: **červený stop** (zaoblený čtverec). Popisek „Zastavit měření“.
- `paused`: červený stop, ztlumený, pod ním „Pozastaveno“ (klik = Stop, jako dosud).
- Ikony jako inline SVG/CSS, dostatečný kontrast ve světlém i tmavém tématu, zachovej focus styl a hover.
- Tray ikona: idle varianta zůstává, recording varianta beze změny.

### Testy (navíc)
- i18n: shodné klíče `cs`/`en`, žádné prázdné hodnoty; volba jazyka podle systému (`cs-CZ` → cs, `sk` → cs, `de-DE` → en, prázdný seznam → en).
- Aplikační vrstva: změna jazyka a tématu přes `settings_save` nehází chyby, tray menu se přestaví v novém jazyce.

## 13. Jeden záznam v My Hours na den a cíl

Lokálně zůstávají všechny úseky přesně jak byly naměřeny, ale do My Hours se pro každou kombinaci **místní den + projekt + úkol + poznámka** zapisuje **jediný záznam**.

- Při startu úseku se vytvoří běžící časovač (`startNewLog`) s počátkem **posunutým zpět** o čas, který se v dané skupině dnes už odpracoval (`shiftMs`). Součet na webu tak odpovídá celkovému času.
- Při stopu se zavolá `stoptimer` se skutečným koncem; délka záznamu = dřívější čas skupiny + právě ukončený úsek.
- Při dalším startu stejné skupiny se nejdřív vytvoří nový běžící záznam (s novým posunem) a teprve potom se smaže předchozí zastavený (`DELETE /Logs/{id}`, operace `delete` ve frontě). Při selhání se tedy čas neztratí; nejhůř zůstane krátce duplicita.
- Smazání, které selže natrvalo (např. 404), se ignoruje. Dočasné chyby se opakují.
- Záznamy se zahazují/spojují stejně i při offline režimu (`insertlog` s posunutým začátkem) a při 404 u `stoptimer`.
- Segmenty vytvořené starší verzí (bez příznaku `merge`) se chovají jako dřív: jeden záznam na úsek. Nový den začíná bez posunu.
- Úseky kratší než 60 s, jejichž start se na server nedostal, se neposílají, ale jejich čas se do součtu skupiny započítává.

## 14. Nečinnost

`idle_watcher` (bez závislosti na OS) každou sekundu čte dobu nečinnosti systému. Při dosažení limitu `idleMinutes` (výchozí 10, 0 = vypnuto) zavolá `tracker.system_pause('idle', now - idle)`, takže úsek skončí v okamžiku posledního vstupu. Při aktivitě (idle < 2 s) nebo vypnutí funkce volá `system_resume`. Hlavní okno ukazuje „Pozastaveno – nečinnost“.

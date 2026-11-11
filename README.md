# TimeWatch

Malá desktopová aplikace (Windows + macOS, [Tauri 2](https://tauri.app) a Rust), která měří čas práce na počítači a synchronizuje ho do [My Hours](https://app.myhours.com). Každý souvislý úsek běhu měření se uloží jako jeden time log.

## Instalace a spuštění

Požadavky: Node.js LTS, [Rust](https://rustup.rs) (stable) a na Windows MSVC C++ Build Tools včetně Windows SDK (workload „Desktop development with C++“) a WebView2 (součást Windows 11).

```
npm install
npm start          # spuštění aplikace (tauri dev)
npm test           # unit testy logiky (cargo test)
npm run lint       # ESLint pro UI + cargo clippy
npm run dist:win   # NSIS instalátor (src-tauri/target/release/bundle/nsis/)
npm run dist:mac   # dmg (musí se sestavit na macOS)
npm run icons      # znovu vygeneruje ikony v assets/
```

Instalátory (Windows i macOS) staví také GitHub Actions (`.github/workflows/build.yml`).

Po prvním spuštění se aplikace (v zabaleném tvaru) sama přidá do spouštění po přihlášení do systému; lze to vypnout v nastavení nebo v menu tray ikony. Při spuštění z `npm start` se automatické spouštění nenastavuje.

## Přihlášení k My Hours

TimeWatch podporuje dva způsoby přihlášení, vždy je aktivní jen jeden (uložení jednoho druhý nahradí).

### 1. Přihlášení účtem My Hours (doporučeno, funguje i na free tarifu)

1. Otevřete Nastavení (ikona ⚙) a klikněte na **Přihlásit se k My Hours**.
2. V otevřeném okně se přihlaste jako ve webové aplikaci (e-mail a heslo, případně Google). Cookies přihlášení zůstávají v samostatném profilu okna, příště stačí obvykle jedno kliknutí.
3. Po přihlášení vyberte **projekt** (a volitelně úkol) a klikněte na **Uložit**.

Aplikace nikdy nevidí vaše heslo, na stránku My Hours nic nevpisuje a nečte ji. Ukládá se jen obnovovací token, v systémovém úložišti přihlašovacích údajů (Windows Credential Manager / macOS Keychain); přístupový token je jen v paměti. **Odhlásit** token zneplatní, smaže ho a vymaže data přihlašovacího okna.

**Upozornění: toto je neoficiální cesta.** TimeWatch používá stejné přihlášení (OpenID Connect, Authorization Code + PKCE, `client_id` webové aplikace) jako web My Hours, protože oficiální API klíč je jen v placených tarifech. My Hours ji může kdykoli změnit, pak přestane fungovat a bude třeba aplikaci upravit.

Pokud Google v okně odmítne přihlášení („nezabezpečený prohlížeč“), přihlaste se e-mailem a heslem k účtu My Hours (u účtu přihlašovaného přes Google si v My Hours nejdřív nastavte heslo). Vyprší-li přihlášení, aplikace ukáže „Přihlášení vypršelo – přihlaste se znovu“; měření se ukládá lokálně a odešle se po novém přihlášení.

### 2. API klíč (placený tarif)

1. Přihlaste se na https://app.myhours.com, v nastavení profilu v části **API** vytvořte **API key**.
2. V Nastavení rozbalte „Mám placený tarif – použít API klíč“, vložte klíč a klikněte na **Ověřit**. Platný klíč se uloží.
3. Vyberte projekt a klikněte na **Uložit**.

Klíč je uložen v systémovém úložišti přihlašovacích údajů. Do okna aplikace se nikdy nevrací celý (zobrazí se jen `••••1234`) a nikdy se nezapisuje do logu.

**Aktualizace ze starší verze:** `state.json` a `settings.json` se při prvním spuštění automaticky zkopírují ze starého umístění. Uložené přihlášení se nepřenáší, po aktualizaci se stačí jednou znovu přihlásit.

Bez přihlášení a projektu okno ukazuje „Nastavte My Hours“ a měření nelze spustit.

## Ovládání

- **Kulaté tlačítko** (nebo mezerník / Enter): Start / Stop. Zastaveno = zelená šipka „play“, běží = červený stop, pozastaveno = ztlumený červený stop s textem „Pozastaveno“. Pod ním je dnešní naměřený čas ve formátu `H:MM`.
- **📌** vždy navrchu (zapamatuje se), **⚙** nastavení, **–** skrýt do oznamovací oblasti. Okno lze přesouvat tažením, pozice se pamatuje.
- Zavření okna aplikaci neukončí, běží dál v tray. Ukončení: tray menu → **Ukončit**.
- Tray menu: Spustit/Zastavit, Zobrazit okno, Vždy navrchu, Nastavení…, Synchronizovat teď, Spouštět po startu systému, Ukončit.

## Jazyk a vzhled

V Nastavení jsou dvě volby, které se projeví okamžitě (bez restartu a bez nutnosti kliknout na Uložit):

- **Jazyk**: Podle systému (výchozí) / Čeština / English. „Podle systému“ použije první preferovaný jazyk systému (čeština a slovenština = čeština, jinak angličtina). Přepne se obě okna, tray menu, tooltip i titulky oken. Logy jsou vždy anglicky.
- **Vzhled**: Podle systému (výchozí) / Světlý / Tmavý.

Všechny texty jsou ve slovnících `locales/cs.json` a `locales/en.json`.

## Chování při zamčení, uspání a vypnutí

Měření má stavy **zastaveno**, **běží** a **pozastaveno**. Zda má měření běžet, určuje jen uživatel (Start / Stop).

- **Zamknutí obrazovky nebo uspání**: aktuální úsek se uzavře a měření se pozastaví. Po odemknutí (nebo probuzení bez zamykací obrazovky) se automaticky otevře nový úsek.
- **Vypnutí / odhlášení počítače**: úsek se uzavře a stav se zapíše na disk. Po dalším startu počítače a aplikace měření pokračuje novým úsekem.
- **Ukončení aplikace** (Ukončit): úsek se uzavře, po příštím spuštění se měření obnoví, pokud běželo.
- **Pád aplikace nebo výpadek proudu**: každých 30 s se zapisuje „heartbeat“. Nezavřený úsek se při příštím startu uzavře časem posledního heartbeatu.
- **Půlnoc**: běžící úsek se rozdělí na 23:59:59.999 a 00:00 (logy se počítají po dnech).
- Změna projektu, úkolu nebo poznámky během měření uzavře úsek a otevře nový s novými hodnotami.

## Synchronizace

Operace (start / stop / vložení hotového logu) se řadí do trvalé fronty a posílají se postupně. Při výpadku sítě se fronta uloží na disk a odešle později; start a stop, které se nestihly odeslat, se sloučí do jednoho `insertlog`. Úseky kratší než 60 s se neodesílají, pokud start ještě neodešel. Dočasné chyby (síť, 5xx, 408, 429) se opakují s prodlevou 30 s až 5 min, synchronizace běží také při každé změně stavu, při startu a periodicky (výchozí 5 minut, nastavitelné 1–60). Neplatný API klíč (401/403) zastaví pokusy do změny klíče. Ostatní chyby 4xx jsou trvalé: operace se označí jako neúspěšná, zapíše se do logu a ukáže se varování.

Stav synchronizace ukazuje barevná tečka u názvu projektu (tooltip s popisem).

## Kde jsou data a logy

Složka uživatelských dat: `%APPDATA%\cz.timewatch.tracker` (Windows), `~/Library/Application Support/cz.timewatch.tracker` (macOS).

| Soubor | Obsah |
|---|---|
| `state.json` | stav měření, úseky (7 dní), fronta synchronizace |
| `settings.json` | projekt, úkol, poznámka, interval, jazyk, vzhled, pozice okna |
| `logs/timewatch.log` | log (rotace při 1 MB, jeden starý soubor `.log.1`) |

Přihlášení (obnovovací token nebo API klíč) je v systémovém úložišti přihlašovacích údajů pod názvem služby „TimeWatch“, ne v souboru.

## Struktura projektu

- `src-tauri/core/` crate `timewatch-core`: čistá logika bez Tauri (`tracker`, `sync_queue`, `api`, `auth`, `secrets`, `power`, `idle_watcher`, `store`, `model`, `i18n`) a její testy
- `src-tauri/src/` aplikace: `app.rs` (propojení), `ui.rs` (okna a tray), `commands.rs` (IPC), `platform/` (nečinnost, zámek obrazovky a události napájení pro Windows a macOS)
- `ui/` okno časovače a nastavení (HTML/CSS/JS) a `tauri-bridge.js`
- `locales/` slovníky textů (cs/en), sdílí je Rust i UI
- `assets/` ikony a skript `generate-icons.js`
- `docs/SPEC.md` specifikace chování, `docs/ARCHITECTURE.md` architektura a pravidla pro kód

## Známá omezení

- Odpovědi `startNewLog` a `tasklist` v My Hours nejsou plně zdokumentované, parsují se defenzivně (viz `src-tauri/core/src/api.rs`). Formát pole `date` (nyní `YYYY-MM-DD`) je na jednom místě, pokud by API vracelo 400.
- **macOS:** kód pro zámek obrazovky a nečinnost je psaný bez přístupu k Macu a ověřuje ho jen CI překladem. Zámek obrazovky se zjišťuje dotazováním jednou za sekundu, uspání se pozná z časové mezery mezi tiky a vypnutí/odhlášení ukončením aplikace. Před nasazením ho vyzkoušejte na skutečném Macu.
- Instalátor není podepsaný, Windows SmartScreen (a na macOS Gatekeeper) proto při prvním spuštění zobrazí varování.

## Jeden záznam na den

Více spuštění a zastavení časovače (včetně zamčení obrazovky) se v My Hours spojí do **jednoho záznamu za den** pro stejný projekt, úkol a poznámku. Při každém dalším startu aplikace vytvoří nový běžící záznam s odpovídajícím posunem a starý zastavený smaže. Záznamy vytvořené starší verzí zůstanou beze změny.

## Nečinnost

Když se určitou dobu nepohne myš ani nestiskne klávesa (výchozí 10 minut, v nastavení lze změnit, 0 = vypnuto), měření se pozastaví. Doba nečinnosti se do času **nepočítá** (úsek se uzavře v okamžiku posledního pohybu) a po prvním pohybu se měření samo obnoví. Pozor: sledování videa nebo čtení bez jakéhokoliv ovládání se počítá jako nečinnost.

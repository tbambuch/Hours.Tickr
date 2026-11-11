# Hours.Tickr: architektura

Chování aplikace popisuje `docs/SPEC.md`. Tento dokument popisuje, jak je kód rozdělený.

## Struktura

```
src-tauri/
  Cargo.toml            workspace + aplikace (crate `timewatch`)
  core/                 crate `timewatch-core`: čistá logika, žádné Tauri/OS API, testovatelná kdekoli
    src/model.rs        State, Segment, Op, Settings, JsonId, Ms + (de)serializace ISO časů
    src/clock.rs        Clock, FakeClock, local_midnight, local_date_str, format_hm
    src/logger.rs       Logger (file/memory/noop) + sanitize (redakce přihlašovacích údajů)
    src/store.rs        Store<T>, SharedState (atomický synchronní zápis JSON)
    src/http.rs         trait Http + ReqwestHttp
    src/error.rs        ApiError, ErrorKind, classify_error
    src/tracker.rs      stavový automat měření
    src/idle_watcher.rs pozastavení při nečinnosti
    src/power.rs        zámek/spánek/vypnutí → tracker
    src/api.rs          klient My Hours API
    src/sync_queue.rs   perzistentní fronta operací + retry
    src/auth.rs         OAuth (PKCE), správa tokenů
    src/secrets.rs      úložiště přihlašovacích údajů (keyring)
    src/i18n.rs         slovníky (locales/*.json)
  src/                  aplikace
    app.rs              propojení částí (Core), přihlášení, ukončení
    ui.rs               okna (časovač, nastavení, přihlášení) a tray
    commands.rs         IPC příkazy volané z oken
    platform/           nečinnost, zámek obrazovky a události napájení (Windows, macOS)
  capabilities/         co smí okna volat
ui/                     okna (HTML/CSS/JS) + tauri-bridge.js (`window.timewatch`)
locales/                cs.json, en.json
```

## Pravidla pro `timewatch-core`

- Čas je všude `Ms` (i64, ms od epochy). Do JSON/API se převádí přes `model::format_iso` a serde moduly `iso_ms`.
- Žádné `unwrap()`/`expect()` na datech z disku, sítě nebo UI. Zámky jsou `parking_lot::Mutex` (bez poisoningu).
- **Zámek stavu (`SharedState`) se nikdy nedrží přes `.await` ani při volání callbacků** (`on_change`, `on_queue_change`,
  `on_status`, `get_target`, `is_locked`). Callbacky obvykle čtou stav zpět, takže by došlo k deadlocku. Vzor: zamkni → změň → `save()` → odemkni → callback.
- Po každém `.await` se segment/operace znovu hledá podle `local_id` / `id`, protože tracker mezitím stav mění.
- Veřejné typy jsou `Send + Sync`, sdílí se přes `Arc`; vše závislé na čase, síti a disku se injektuje (testy používají `FakeClock`, mock `Http`, `tempfile`).
- Časování v testech: `#[tokio::test(start_paused = true)]` + `tokio::time::advance`.
- Logy jsou anglicky a nikdy neobsahují tokeny, API klíč, `code`, `code_verifier` ani hlavičku Authorization.

## Pravidla pro aplikaci

- Okna se nesmí vytvářet z hlavního vlákna (synchronní příkazy, handlery menu), jinak se na Windows zablokuje načtení webview. Vytváření probíhá v asynchronním úkolu (`ui::open_settings`).
- Jen okna `timer` a `settings` smějí volat příkazy; přihlašovací okno zobrazuje vzdálený obsah a nemá žádnou capability.

## Data

Data jsou v `%APPDATA%\cz.timewatch.tracker` (macOS `~/Library/Application Support/cz.timewatch.tracker`): `state.json`, `settings.json`, `logs/`.
Při prvním startu se `state.json` a `settings.json` zkopírují ze starého umístění `…/TimeWatch`, pokud nové neexistují. Přihlašovací údaje jsou v systémovém úložišti (služba „TimeWatch“), starší uložená přihlášení se nepřenášejí.

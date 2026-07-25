# rpi — секретные файлы (`rpi secrets`) (дизайн)

Дата: 2026-07-07. Базовые спеки: `2026-06-09-pi-deploy-tool-design.md` (env-бандл,
деплой-пайплайн), `2026-06-11-pi-secrets-ingress-v0.2.md` (шифрованное хранилище).

**Критерий готовности:** проект с `[secrets] files = ["certs/server.pem"]` в
`rpi.toml` после `rpi secrets send` и `rpi deploy` получает файл в чекауте на
плате по тому же относительному пути; env-workflow (`.env`) работает через ту же
команду; `rpi env send` / `rpi env ls` удалены.

## 1. Скоуп

Входит:

- Новая секция `[secrets]` в `rpi.toml`: `env` (путь к env-файлу, дефолт `.env`)
  и `files` (список относительных путей секретных файлов, дефолт `[]`).
- Команды `rpi secrets send [--apply]` и `rpi secrets ls`, заменяющие
  `rpi env send [--apply]` и `rpi env ls`.
- Единый бандл секретов на агенте: env-переменные + файлы в одном
  зашифрованном блобе, атомарная замена всего набора при каждом send.
- Материализация файлов при деплое по тем же относительным путям внутри
  чекаута проекта (`<data_dir>/workdirs/<project>/`).
- Поддержка бинарных файлов (base64 на проводе).
- Fallback-чтение старого `<project>.env.age`, чтобы уже задеплоенные проекты
  не потеряли секреты при обновлении агента.

НЕ входит (YAGNI):

- Маппинг «локальный путь → другой путь на плате». Пути идентичны с обеих
  сторон — как попросил пользователь.
- Права/exec-биты файлов. Всё пишется 0600, каталоги 0700. — superseded by
  `docs/superpowers/specs/2026-07-25-secret-file-modes-design.md`: secret
  files now default to 0644 (or `[secrets].file_mode`) so a container that
  isn't the agent's uid can read them; `.env` stays 0600.
- Частичная отправка/обновление одного файла. Только замена всего бандла.
- Маскирование содержимого файлов в логах. Содержимое файлов в логи не
  попадает вообще; маскер, как и раньше, вооружается только env-значениями.
- Алиасы или скрытая совместимость для `rpi env ...` в CLI и роута
  `/v1/projects/{name}/env` на агенте. Чистый разрыв.
- Шифрование файлов внутри workdir. Контейнерам нужен plaintext; защита — права
  файлов и владение `rpi-agent`.

## 2. Ключевые решения

1. **Единый бандл.** Одна сущность `SecretsBundle { vars, files }`, один
   эндпоинт `PUT /v1/projects/{name}/secrets`, один блоб
   `<project>.secrets.age`. Env и файлы синхронизируются атомарно — нет
   состояния «env новый, файлы старые».
2. **`secrets` вместо `env`, без алиасов.** Команды и роуты `env` удаляются.
   Одна ментальная модель «секреты проекта» вместо двух.
3. **`[env]` в rpi.toml → жёсткая ошибка парсинга** с подсказкой «замените
   [env] на [secrets]». Молчаливое игнорирование опасно: секреты перестали бы
   отправляться, а заметили бы это только по сломанному деплою.
4. **Правило env-файла:** указан явно — обязан существовать (ошибка, если
   нет); действует дефолт `.env` — отсутствие допустимо (проект только с
   файлами не обязан иметь `.env`).
5. **Whole-bundle replace.** Каждый send атомарно заменяет весь сохранённый
   набор (семантика текущего env-бандла): убрал путь из `files` и переслал —
   на агенте файла больше нет.
6. **Записи только внутрь workdir.** Пути валидируются дважды (CLI и агент):
   только относительные, без `..`, без NUL. Перед записью `SecretsWriter`
   канонизирует родительский каталог цели и проверяет, что тот остался внутри
   канонизированного workdir — симлинк в репозитории не может увести запись
   наружу; симлинк на пути = ошибка стадии деплоя.
7. **Лимиты: 1 MiB на файл, 8 MiB на бандл** (декодированные байты),
   проверяются на CLI и повторно на агенте. Body-limit axum поднимается до
   12 MiB (base64 раздувает ~на треть).
8. **Legacy fallback при load.** `load` ищет `<project>.secrets.age`; если нет —
   читает старый `<project>.env.age` (dotenv) как бандл без файлов. Следующий
   `save` пишет новый формат и удаляет старый файл. Пересылать секреты после
   обновления агента не обязательно.
9. **Различение «старый агент» от «проект не найден».** Ответы агента всегда
   несут JSON `{"error": ...}`; 404 несуществующего роута у axum — с пустым
   телом. 404 без JSON-тела на `/secrets` → CLI печатает «агент не
   поддерживает secrets API — обновите rpi на плате».

## 3. Поверхность: CLI и rpi.toml

```toml
[secrets]
env = ".env"                    # опционально, дефолт ".env"
files = [                       # опционально, дефолт []
  "certs/server.pem",
  "config/service-account.json",
]
```

- `rpi secrets send [--apply]` — читает `[secrets]`, собирает env-файл и все
  перечисленные файлы, шлёт одним PUT. `--apply`: как у нынешнего
  `env send --apply` — переписать секреты в workdir запущенного проекта,
  переписать override и `docker compose up`.
- `rpi secrets ls` — ключи env-переменных и пути файлов. Значения и
  содержимое — никогда.
- Пути в `files` — только с прямыми слэшами (`certs/server.pem`), в том числе
  на Windows-машине разработчика; бэкслэш → ошибка парсинга. На локальном
  диске CLI преобразует их в нативный путь при чтении.
- Дубликаты путей в `files` → ошибка парсинга.
- Пустой бандл (нет ни vars, ни files) → ошибка до сети, как сейчас.
- Отсутствующие файлы из `files` перечисляются в ошибке все сразу.

## 4. Протокол (`crates/bin/src/proto.rs`)

```
PUT /v1/projects/{name}/secrets
  req : { "vars": {"KEY":"VALUE",...},
          "files": {"certs/server.pem":"<base64>",...},
          "apply": false }
  resp: { "saved_keys": N, "saved_files": M, "applied": bool }

GET /v1/projects/{name}/secrets
  resp: { "keys": ["KEY1",...], "files": ["certs/server.pem",...] }
```

Типы: `SecretsSendRequest`, `SecretsSendResponse`, `SecretsListResponse`.
Роуты `/v1/projects/{name}/env` и типы `EnvSendRequest`/`EnvSendResponse`/
`EnvKeysResponse` удаляются.

## 5. Домен и use-cases

- `crates/domain/src/entities.rs`: `EnvBundle` → `SecretsBundle
  { vars: BTreeMap<String,String>, files: BTreeMap<String,Vec<u8>> }`
  (ключ files — относительный путь). Кастомный `Debug` — только имена ключей,
  пути и счётчики.
- `crates/domain/src/contracts.rs`: `SecretStore` сохраняет форму
  (`save`/`load`/`remove`), работает с `SecretsBundle`. `EnvFileWriter` →
  `SecretsWriter` с `write(workdir, &SecretsBundle)`: пишет `.env` (если vars
  непусты) и каждый файл по его пути.
- `crates/application/src/env.rs` → `secrets.rs`: `SendEnv` → `SendSecrets`,
  `ListEnvKeys` → `ListSecrets`. Логика 1-в-1 текущая: save; при apply — load
  проекта (404 если не задеплоен), вооружить маскер vars, переписать секреты в
  workdir, override, `runtime.up`.

## 6. Хранение на агенте (`crates/infrastructure/src/secrets.rs`)

- `EncryptedFileStore` пишет `<data_dir>/secrets/<project>.secrets.age`:
  внутри JSON `{ "vars": {...}, "files": {"path": "<base64>"} }`, шифрование
  тем же age-ключом (`<data_dir>/secret.key`), запись через существующий
  `fsutil::write_private_atomic` (0600, tmp+rename).
- `validated_project` остаётся защитой от traversal в имени проекта.
- `load`: `.secrets.age` → нет? → legacy `.env.age` (dotenv → vars) → нет? →
  пустой бандл. `save`: пишет `.secrets.age`, удаляет `.env.age`, если тот
  есть. `remove`: удаляет оба.

## 7. Стадия деплоя и запись в workdir

- `crates/application/src/deploy.rs`, существующая стадия `secrets` (между
  `fetch` и `override`): load бандла → если непуст: вооружить маскер (vars) →
  `SecretsWriter.write(&fetched.workdir, &bundle)` → лог
  `secrets injected (N keys, M files)`. Порядок стадий не меняется.
- `crates/infrastructure/src/envfile.rs` → `secretsfile.rs`
  (`FsSecretsWriter`): `.env` — как сейчас (0600, атомарно); файлы — создать
  родительские каталоги (0700), канонизировать родителя, проверить префикс
  канонизированного workdir, записать 0600 атомарно. Нарушение → `DomainError`
  → стадия падает, деплой failed.

## 8. Модель безопасности

- Агент — systemd-сервис под непривилегированным `rpi-agent` (не в Docker);
  писать за пределы владений `rpi-agent` не может на уровне ОС.
- API доступен только через unix-сокет `/run/rpi/agent.sock` (каталог 0750):
  нужен SSH-доступ к плате и членство в группе.
- Фича не расширяет периметр: кто может дёргать API, тот и так деплоит
  произвольный compose (доступ к Docker).
- Три слоя против записи «куда угодно»: валидация путей (CLI + агент),
  канонизация против симлинков, права процесса `rpi-agent`.
- Секреты на диске агента — только зашифрованные; в workdir — plaintext 0600
  (нужны контейнерам), читает только `rpi-agent`.

## 9. Ошибки

| Ситуация | Поведение |
| --- | --- |
| `[env]` в rpi.toml | ошибка парсинга с подсказкой про `[secrets]` |
| явный `env` не существует | ошибка CLI |
| дефолтный `.env` отсутствует | пропустить молча |
| файл из `files` не найден | ошибка CLI со списком всех отсутствующих |
| абсолютный путь / `..` / NUL | ошибка CLI; агент повторно → 400 |
| превышение лимитов | ошибка CLI; агент повторно → 400 |
| пустой бандл | ошибка CLI до сети |
| не-base64 на агенте | 400 с указанием пути |
| `--apply`, проект не задеплоен | 404 (как сейчас) |
| 404 без JSON-тела на `/secrets` | «обновите rpi на плате» |
| симлинк/эскейп при записи на деплое | стадия `secrets` failed с понятной строкой |

## 10. Тестирование

Все тесты in-file `#[cfg(test)]`, по существующим паттернам (mockall-моки
доменных трейтов, `CollectSink`, tower `oneshot`, tempdir):

- `rpitoml.rs`: `[secrets]` с дефолтами; `[env]` → ошибка с подсказкой;
  валидация путей; `init.rs` рендерит `[secrets]` и round-trip через parse.
- `commands.rs`: правила сборки бандла (явный env отсутствует → ошибка;
  дефолтный отсутствует при непустых files → ок; список пропавших; лимиты).
- application `secrets.rs`: save без apply; apply переписывает и `up` с
  маскированными vars; ls — только имена/пути; пустой бандл отклонён.
- `deploy.rs`: файлы записаны в workdir; лог `secrets injected (...)`;
  порядок стадий прежний.
- infrastructure `secrets.rs`: round-trip vars + бинарные files; на диске не
  plaintext; fallback с `.env.age` и его удаление при следующем save; 0600;
  traversal отклонён.
- `secretsfile.rs`: `.env` + файлы записаны; каталоги 0700, файлы 0600;
  симлинк в родителе отклонён (unix-тест); `..`/абсолютные отклонены.
- `http.rs`: send→ls round-trip; 400 на плохие ключи/пути/base64/размер;
  роут `/env` теперь 404; `--apply` незадеплоенного → 404.
- `main.rs`: `secrets send --apply` и `secrets ls` парсятся; `env send`
  больше не существует.

## 11. Документация и миграция

- README: заменить `env send`/`env ls` на `secrets`, описать `[secrets]`.
- `rpi init` (`crates/bin/src/cli/init.rs`): генерировать `[secrets]`.
- Миграционная заметка (в духе `docs/migration-v0.5-to-v0.6.md`):
  1) обновить агент на плате; 2) в rpi.toml заменить `[env] file = "..."` на
  `[secrets] env = "..."`; 3) пересылать секреты не обязательно — агент читает
  старый `.env.age` как fallback; 4) старый CLI с новым агентом (и наоборот)
  для секретов несовместим.
- После имплементации обновить пользовательские скиллы `rpi-cli` / `rpi-toml`
  (описывают `rpi env send` и `[env]`).

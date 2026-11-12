# SqlToy Navigator for OpenIDEA

Jump between Java sqlId strings and their SqlToy XML SQL definitions.

Java 代码通过字符串引用 SqlToy SQL——`sqlToyLazyDao.findBySql("app_order_list", …)`，
SQL 定义在 `*.sql.xml` 文件的 `<sql id="app_order_list">` 里。本插件在两者之间建立索引，
让 Go to Definition、Ctrl/⌘-Click、Find Usages、Rename 跨 Java/XML 边界解析，
在 `…BySql("` 内补全 sqlId，并标出引用不存在的 sqlId。

## Features

| Feature | How |
|---|---|
| Java sqlId → XML definition | Go to Definition / Ctrl-Click on the sqlId string |
| XML `<sql id>` → Java usage | Ctrl-Click on the id value, or the right-click action *Find Java Usages of This sqlId* |
| XML gutter arrow | on every `<sql id>` that a Java call uses |
| sqlId completion | inside the string argument of a `…Sql(…)` DAO call |
| Undefined sqlId | red squiggle + Problems entry (Error) |
| Find Usages / Rename | host-side, from the indexer graph (API 1.3+ host feature) |

### Right-click action

`SqlToy Navigator: Find Java Usages of This sqlId` — put the caret on a
`<sql id>` value in an XML file and run it: with one use site it jumps
straight there, with several it jumps to the first and lists the rest in a
notification.

## Capabilities

The plugin asks for two low-risk read-only capabilities:

- `project.read` — to search the project's Java files for sqlId use sites.
- `editor.read` — to read the focused editor's text and caret for the
  right-click action.

It asks for no network, process, filesystem-write or credential access.

## Recognized shapes

- XML: `*.sql.xml` files (or any file with a `<sqltoy>` root) — every
  `<sql id="…">` becomes a definition under the key `sqltoy#<id>`.
- Java: every call of a SqlToy DAO method whose name ends in `Sql`
  (`findBySql`, `getBySql`, `findTopBySql`, `updateBySql`, `executeSql`,
  `findPageBySql`, …) whose first *string* argument is a sqlId. The sqlId may
  sit behind other arguments (`findPageBySql(pageModel, "id", …)`). String
  literals and comments never produce matches.

## Build

```sh
cargo test                     # logic, against the SDK's mock host
openidea-plugin build          # → dist/plugin.wasm, then `check`
openidea-plugin link           # load this folder in the IDE
```

Requires the `wasm32-wasip2` Rust target and the `openidea-plugin` CLI
(from `cargo install --path crates/ide-host --bin openidea-plugin` in an
OpenIDEA checkout).

## Licence

Apache-2.0. See [LICENSE](LICENSE).

# Locale modules

Each locale directory (`en/`, `es/`, `fr/`) holds one JSON file per translation
module. A module owns every key sharing its dot prefix: `video.json` owns
`video.*`, `status.json` owns `status.*`, and so on. Keys keep their full
dotted form inside the module files; the loader merges all modules of a locale
into a single catalog at compile time.

Rules:

- Every module file must exist in all three locale directories with identical
  key sets (checked by `locale_catalogs_cover_the_same_keys`).
- No key may appear in two modules of the same locale (checked by
  `locale_modules_do_not_shadow_keys`).
- Every `<module>.json` file on disk must be listed in the `define_catalog!`
  invocation in `src/lib.rs` (checked by `every_module_file_is_loaded`).

Adding a module:

1. Create `locales/en/<module>.json`, `locales/es/<module>.json`, and
   `locales/fr/<module>.json` with the same keys.
2. Add `"<module>"` to the `define_catalog!` list in alphabetical order.
3. Run `cargo test -p pw-graph-i18n`.

Written by Chrome 158 headless on origin `https://desktop.tidal.com` (a local HTTPS server mapped with
`--host-resolver-rules="MAP desktop.tidal.com 127.0.0.1:8443, MAP * ~NOTFOUND" --ignore-certificate-errors`), four
launches on one `--user-data-dir`, one page each. The 12 padding values push each launch's writes out of the log into a
table. `U` stands for `0f1e2d3c-4b5a-4978-8796-a5b4c3d2e1f0`.

1. `ls/a1`..`ls/a8` = `va1`..`va8`, `ls/del_only` = `x`, `ls/del_rewrite` = `old`, `AuthDB/u1Data` = 40 x `A`;
   for each id of `U`, `U_clientCredentials_PROD` and `tidal`, `AuthDB/<id><s>` = `<id>:<s>` with `<s>` in `Salt`,
   `Counter`, `Key`, `Data`; `AuthDB/goneData` = `G`, `AuthDBx` = `neighbour`, `authdb/y` = `neighbour`; then
   `pad1_0`..`pad1_11`, each 10 000 x `v` followed by its index.
2. `removeItem` of `ls/del_only`, `ls/del_rewrite`, `AuthDB/goneData` and `AuthDB/tidalData`, `ls/zb1` = `vb1`,
   `pad2_*`.
3. `ls/del_rewrite` = `new`, `AuthDB/tidalData` = `tidal:Data2`, `ls/zc1` = `vc1`, `pad3_*`.
4. `ls/d1` = `vd1`, `pad4_*`.

Keep `000003.ldb`, `000006.ldb`, `000009.ldb`, `CURRENT` and `MANIFEST-000001` from `Default/Local Storage/leveldb`;
the tombstones must sit in the second table.

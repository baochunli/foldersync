// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { expect, open, test } from "nsh/test";

test("shows the prototype", async () => {
  const app = await open(".");
  expect(app.texts()).toContain("FolderSync — prototype");
});

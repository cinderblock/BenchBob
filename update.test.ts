import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { cloudInitWriteFiles, compareVersions, firstrunInstall } from "./update.ts";

const KEY = readFileSync(`${import.meta.dir}/packaging/benchbob-archive-keyring.asc`, "utf8");

test("cloud-init write_files carry the apt source and the exact key", () => {
  const doc: any = Bun.YAML.parse(cloudInitWriteFiles().join("\n"));
  const files = Object.fromEntries(doc.write_files.map((f: any) => [f.path, f.content]));
  expect(files["/usr/share/keyrings/benchbob-archive-keyring.asc"]).toBe(KEY.trimEnd() + "\n");
  // Not benchbob.sources, the package's conffile: dpkg would stop to ask about it.
  expect(files["/etc/apt/sources.list.d/benchbob.sources"]).toBeUndefined();
  const src = files["/etc/apt/sources.list.d/benchbob-bootstrap.sources"];
  expect(src).toContain("URIs: https://cinderblock.github.io/BenchBob/apt");
  expect(src).toContain("Signed-By: /usr/share/keyrings/benchbob-archive-keyring.asc");
});

test("firstrun fragment writes the key verbatim and enables the install unit", () => {
  const sh = firstrunInstall();
  const key = /cat > '\/usr\/share\/keyrings\/benchbob-archive-keyring\.asc' << 'BBKEYEOF'\n([\s\S]*?)\nBBKEYEOF\n/.exec(sh);
  expect(key?.[1]).toBe(KEY.trimEnd());
  expect(sh).toContain("systemctl enable benchbob-firstboot.service");
  expect(sh).toContain("ConditionPathExists=!/usr/lib/benchbob/VERSION");
});

test("compareVersions orders releases, pre-release builds and post-release builds", () => {
  const ordered = ["0.0.0~git85.83b68de", "0.0.0~git86.a1b2c3d", "0.1.0~git90.ffffff0", "0.1.0", "0.1.0+git4.abc1234", "0.1.0+git12.0000000", "0.1.1", "0.2.0", "1.0.0"];
  for (let i = 0; i < ordered.length; i++) {
    for (let j = 0; j < ordered.length; j++) {
      expect(Math.sign(compareVersions(ordered[i], ordered[j]))).toBe(Math.sign(i - j));
    }
  }
});

test("compareVersions accepts a v prefix and treats unparseable versions as equal", () => {
  expect(compareVersions("v0.2.0", "0.1.0")).toBeGreaterThan(0);
  expect(compareVersions("dev", "0.1.0")).toBe(0);
});

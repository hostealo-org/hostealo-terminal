import { describe, expect, it } from "vitest";

import { mayHoldSecrets } from "./fileSecrets";

describe("files that may hold secrets", () => {
  it("knows them by name", () => {
    for (const path of ["/srv/app/.env", "/srv/app/.env.production", "/var/www/wp-config.php", "/root/.my.cnf", "/home/ana/.pgpass", "/home/ana/.ssh/id_ed25519", "/etc/ssl/private/site.key", "/etc/shadow"]) {
      expect(mayHoldSecrets(path, ""), path).toBe(true);
    }
    expect(mayHoldSecrets("/etc/nginx/nginx.conf", "worker_processes auto;\n")).toBe(false);
  });

  it("finds them in the content", () => {
    for (const content of [
      "DB_HOST=127.0.0.1\nDB_PASSWORD=hunter2\n",
      "export AWS_SECRET_ACCESS_KEY=abc\n",
      "SECRET_KEY = 'django-insecure-x'\n",
      "database:\n  password: s3cret\n",
      '{ "apiKey": "sk-123" }',
      "$config = ['password' => 'x'];",
      "define( 'DB_PASSWORD', 'x' );",
      "DATABASE_URL=postgres://app:x@db:5432/app\n",
      "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n",
    ]) {
      expect(mayHoldSecrets("/srv/app/config", content), content).toBe(true);
    }
  });

  it("leaves ordinary files alone", () => {
    for (const content of [
      "server {\n    listen 80;\n    proxy_pass http://127.0.0.1:3000;\n}\n",
      "#!/usr/bin/env bash\nset -euo pipefail\ndocker compose pull web\n",
      "const token = await getToken();\nconst password = form.password;\n",
      "password=\n# secret: see the vault\n",
      "https://example.com/docs\n",
    ]) {
      expect(mayHoldSecrets("/srv/app/file", content), content).toBe(false);
    }
  });
});

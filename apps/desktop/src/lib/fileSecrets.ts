// Whether a file open in the editor may hold passwords or keys, so the AI assistant does not send
// it along with a question unless the user asks for it. A guess on the side of caution: a false
// alarm costs a click, a missed one sends a password to the AI provider.

/** Files that are about secrets by their name alone. */
const SECRET_NAME =
  /^(\.env(\..+)?|.+\.env|wp-config\.php|\.?my\.cnf|\.pgpass|\.netrc|\.htpasswd|shadow|gshadow|credentials(\..+)?|id_(rsa|dsa|ecdsa|ed25519)|secrets?\.(ya?ml|json|env|toml)|.+\.(pem|key|p12|pfx|kdbx))$/i;

/** What a password, key or token looks like in config files and code. */
const SECRET_CONTENT = [
  /-----BEGIN [A-Z ]*PRIVATE KEY-----/,
  // `DB_PASSWORD=…`, `secret_key: …`, `export API_KEY=…` at the start of a line, with a value.
  /^\s*(export\s+)?[\w.-]*(passw(or)?d|pwd|secret|api[_-]?key|access[_-]?key|auth[_-]?token|private[_-]?key)[\w.-]*\s*[=:]\s*["']?[^\s"'#]/im,
  // `"password": "…"`, `'secret' => '…'`.
  /["'][\w.-]*(passw(or)?d|secret|api[_-]?key|access[_-]?key|auth[_-]?token)[\w.-]*["']\s*(=>|[=:])\s*["'][^"']/i,
  // PHP's `define('DB_PASSWORD', '…')` (wp-config and the like).
  /define\(\s*["'][\w]*(PASSWORD|SECRET|KEY|SALT)[\w]*["']\s*,\s*["'][^"']/i,
  // Credentials inside a URL: `mysql://user:pass@host`.
  /\b[a-z][\w+.-]*:\/\/[^\s/:@]+:[^\s/@]+@/i,
];

export function mayHoldSecrets(path: string, content: string): boolean {
  const name = path.split(/[\\/]/).pop() ?? path;
  return SECRET_NAME.test(name) || SECRET_CONTENT.some((pattern) => pattern.test(content));
}

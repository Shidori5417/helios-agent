# Plugin Registry and Trust

The plugin registry is a signed JSON envelope. The envelope contains `schema_version`, `registry_id`, monotonic `sequence`, Unix-second `generated_at`/`expires_at`, sorted `entries`, `key_id`, and an Ed25519 `signature`. The signature covers deterministic payload fields and the sorted entry list. Entry signatures remain required and cover the entry fields followed by manifest bytes.

The trust store contains public keys only:

```json
{
  "keys": {"release-key-1": "<64 hex characters>"},
  "revoked": [],
  "disabled": []
}
```

Use `agent-cli plugins verify --registry registry.json --trust-store trust.json --id example` before installation. `verify`, `install`, and `update` validate the envelope signer, freshness, sequence state, hash, exact key id, revocation state, entry signature, manifest schema, and identity before accepting a registry. The highest accepted sequence and payload digest are atomically stored in `.agent/registry-state.json`; lower sequences and same-sequence equivocation are rejected. Verification reports signer, sequence, and expiry without exposing secrets. Installation then atomically moves a single `plugin.json` into `.agent/plugins/<id>`. `uninstall --id <id>` moves the exact directory to `.agent/plugins/.trash`; it does not recursively delete arbitrary paths.

Local registry files and HTTPS sources are supported. Plain HTTP, archive extraction, symlinked plugin destinations, path traversal, oversized responses, and literal private signing keys are rejected. The repository does not generate or store signing keys.

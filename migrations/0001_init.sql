-- Prompt and response bodies only ever reach this table as age ciphertext.
-- Every other column is operational metadata and is stored in clear so the
-- request list can be rendered and filtered without decrypting anything.
CREATE TABLE requests (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at        INTEGER NOT NULL, -- unix milliseconds
    client            TEXT    NOT NULL, -- API key name, never the key itself
    endpoint          TEXT    NOT NULL,
    model             TEXT    NOT NULL,
    status            TEXT    NOT NULL, -- pending | completed | failed | interrupted
    http_status       INTEGER,
    error             TEXT,             -- fixed category, never upstream content
    request_enc       BLOB    NOT NULL,
    response_enc      BLOB,
    request_bytes     INTEGER NOT NULL,
    response_bytes    INTEGER,
    latency_ms        INTEGER,
    ollama_total_ms   INTEGER,
    ollama_load_ms    INTEGER,
    prompt_tokens     INTEGER,
    completion_tokens INTEGER,
    tokens_per_second REAL
);

CREATE INDEX requests_created_at ON requests (created_at DESC);

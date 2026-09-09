# embedding Specification

## Purpose

The local embedding pipeline: the ONNX Runtime lifecycle (external library), the model manager (download/caching per onnx.yaml), tokenization, the embedding cache, and a provider with batch inference and L2 normalization for ingestion.

## Requirements

### Requirement: ONNX Runtime library

The `embedding` crate provides ONNX Runtime as an EXTERNAL shared library (`.so`/`.dylib`/`.dll`): on first use the library is downloaded per the `onnx.yaml` configuration (the runtime section, the entry for the current platform), unpacked from the archive (zip/tgz), and cached in the data directory. Subsequent runs use the cached library without downloading. The library is loaded by an explicit path from the cache (not through the system search paths).

#### Scenario: First run — library missing
- **WHEN** the library provision is invoked and the cache is empty
- **THEN** the library is downloaded from the onnx.yaml URL for the current platform, unpacked, cached, and the path to it is returned

#### Scenario: Repeat run — library in cache
- **WHEN** the library provision is invoked and the cache already contains the installed library (the version matches onnx.yaml)
- **THEN** no download is performed and the path to the cached library is returned

#### Scenario: Library download error
- **WHEN** the library download or unpacking fails
- **THEN** an explicit error naming the cause is returned; partially downloaded files do not remain in the cache as installed

### Requirement: Model manager

The `embedding` crate manages the embedding models from the `onnx.yaml` registry (the models section): by model name it returns the path to the installed model, and when absent it downloads all model files (per the files list with URLs and sizes) into the data directory and marks the model installed. An unknown model name is an error. Installation is verified by the install cache and the presence of the files.

#### Scenario: Model already installed
- **WHEN** a model marked installed is requested and its files are present
- **THEN** the path to the model directory is returned without downloading

#### Scenario: Model not installed
- **WHEN** a model from the registry is requested that is absent from the cache
- **THEN** all model files are downloaded from the onnx.yaml URLs, the model is marked installed, and the path is returned

#### Scenario: Unknown model name
- **WHEN** a model absent from the onnx.yaml registry is requested
- **THEN** an explicit error is returned and no download is performed

#### Scenario: Default model
- **WHEN** no model name is given
- **THEN** the `default` model from onnx.yaml is used

### Requirement: File downloader

The file downloader downloads files over HTTPS/HTTP with retries (default 3, with a delay), a request timeout, SSRF protection (private/loopback/link-local addresses are rejected), and a progress indicator. After downloading, the file size is checked against the expected value from onnx.yaml; on a mismatch the file is not considered installed.

#### Scenario: Successful download
- **WHEN** a file is downloaded from a reachable URL
- **THEN** the file is saved to the target path, the size matches the expected value, and the progress is displayed

#### Scenario: Transient network error
- **WHEN** the first request fails with a network error
- **THEN** a retry is performed (up to the limit) and on success the file is saved

#### Scenario: Retries exhausted
- **WHEN** all attempts fail
- **THEN** an explicit error is returned and the partial file is deleted

#### Scenario: SSRF protection
- **WHEN** the URL points to a private/loopback/link-local address
- **THEN** the download is rejected with an error before the request is made

#### Scenario: Size mismatch
- **WHEN** the downloaded file has a size different from the expected value in onnx.yaml
- **THEN** a verification error is returned and the file is not marked installed

### Requirement: Tokenization

The tokenizer loads a vocabulary in the HuggingFace `tokenizer.json` format (shipped with the model) and converts text into token IDs and an attention mask for the model. The sequence length is limited to the model's maximum length (default 512); the inverse conversion of tokens to text is supported.

#### Scenario: Text encoding
- **WHEN** text is encoded by the tokenizer
- **THEN** token IDs and an attention mask compatible with the model input are returned

#### Scenario: Length limit
- **WHEN** the text is longer than the model's maximum length
- **THEN** the sequence is truncated to the maximum length

#### Scenario: Decoding
- **WHEN** token IDs are decoded back
- **THEN** the text is returned (accounting for the special tokens)

### Requirement: Embedding cache

The embedding cache stores computed vectors in memory: the key is formed from the model name, the dimensionality, and the text; the cache is size-limited (default 10000 entries) and thread-safe. A repeat request for the same text with the same model returns the vector from the cache without re-inference.

#### Scenario: Cache hit
- **WHEN** a vector is requested for text already computed for the same model and dimensionality
- **THEN** the cached vector is returned without re-inference

#### Scenario: Miss and fill
- **WHEN** a vector is requested for new text
- **THEN** the vector is computed and stored in the cache

#### Scenario: Size limit
- **WHEN** the cache reaches its maximum size
- **THEN** the cache is evicted (old entries are removed) and new entries continue to work

### Requirement: Embedding provider

The provider generates embeddings for a list of texts: each text is tokenized, model inference is run, the vector is normalized (L2) and returned. The vector dimensionality matches the model (bge-m3 — 1024). The cache is checked before inference; the results are stored in the cache. An empty list of texts is an error. The provider is created explicitly (the model is loaded only at provider creation/use, not on the query path).

#### Scenario: Embedding generation
- **WHEN** the provider is given a list of texts
- **THEN** an L2-normalized vector of the model's dimensionality is returned for each text

#### Scenario: Empty list
- **WHEN** the provider is given an empty list of texts
- **THEN** an explicit error is returned

#### Scenario: Cache integration
- **WHEN** some of the texts are already computed
- **THEN** the cached vectors are returned for them and inference is run only for the new ones

#### Scenario: Explicit provider creation
- **WHEN** the provider is not created
- **THEN** the model and the library are not loaded (the query path does not load the embedding model)

### Requirement: Provider model resolution

The embedding provider SHALL be built exclusively from the `onnx.yaml` registry entry for the selected model name: the vector dimension from the entry's `vector_dim`, the model file and the tokenizer (`tokenizer.json`) from the entry's `files[]` list. No alternative resolution path (explicit file paths in the main config) SHALL exist. A registry entry with a non-positive `vector_dim` SHALL be rejected with a configuration error naming the model.

#### Scenario: Resolution from the registry entry
- **WHEN** the provider is built for a model present in the registry
- **THEN** the model file, the tokenizer, and the vector dimension all come from that registry entry, and the provider reports the entry's dimension

#### Scenario: Tokenizer missing from the entry
- **WHEN** the selected registry entry has no `tokenizer.json` in its `files[]` list (or the file is absent from the model directory)
- **THEN** provider construction fails with an explicit error naming the model and the missing file

#### Scenario: Non-positive dimension
- **WHEN** the selected registry entry declares `vector_dim <= 0`
- **THEN** provider construction fails with a configuration error naming the model

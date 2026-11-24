#!/usr/bin/env python3
"""Reference e5 embeddings for the Rust parity test (tests/embed_parity.rs).

Synthetic texts only. Needs sentence-transformers (torch, transformers); no network.
Usage: python -I scripts/e5_reference.py E5_SMALL_DIR E5_BASE_DIR > tests/fixtures/e5_reference.json
Each dir is a full Hugging Face snapshot at the pinned revision (modules.json, 1_Pooling, tokenizer files, ...).
"""

import json
import sys

import sentence_transformers
import torch
import transformers

REVISIONS = {
    "e5-small": "614241f622f53c4eeff9890bdc4f31cfecc418b3",
    "e5-base": "d128750597153bb5987e10b1c3493a34e5a4502a",
}
PREFIX = {"query": "query: ", "passage": "passage: "}
LONG = " ".join(
    f"Paragraph {i}: the quarterly report lists invoices, deliveries and support tickets for region {i % 7}."
    for i in range(80)
)
TEXTS = [
    ("query", "how do I reset my password?"),
    ("passage", "To reset your password, open the account settings, choose Security and follow the link sent by e-mail."),
    ("passage", "Pour réinitialiser votre mot de passe, ouvrez les paramètres du compte puis l'onglet Sécurité."),
    ("passage", "The weather tomorrow will be sunny with a light breeze along the coast."),
    ("query", "réunion budget trimestriel"),
    ("passage", "La réunion sur le budget du troisième trimestre est déplacée à jeudi 14h30, salle B."),
    ("passage", LONG),
    ("passage", "Rendez-vous le 12/03/2026 à 14h30 — café ☕ 東京 naïve façade Ærøskøbing €42,50."),
    ("passage", "  multiple   spaces\n\nand newlines\tand tabs  "),
    ("query", ""),
]


def embed(model_dir):
    # modules.json of the e5 repos: Transformer -> mean Pooling -> Normalize.
    model = sentence_transformers.SentenceTransformer(model_dir, device="cpu", local_files_only=True)
    assert model.max_seq_length == 512, model.max_seq_length
    items = []
    for kind, text in TEXTS:
        # Same input as the Rust side: prefix + trimmed text (sentence-transformers then strips
        # the whole string), one text per batch (no padding).
        s = PREFIX[kind] + text.strip()
        ids = model.tokenize([s])["input_ids"][0].tolist()
        v = model.encode([s], normalize_embeddings=True, convert_to_numpy=True)[0]
        items.append({"kind": kind, "text": text, "ids": ids, "embedding": [round(float(f), 7) for f in v]})
    arch = model[0].auto_model.config.architectures[0]
    return {"architecture": arch, "dims": len(items[0]["embedding"]), "items": items}


def main():
    out = {
        "generator": {
            "script": "scripts/e5_reference.py",
            "sentence_transformers": sentence_transformers.__version__,
            "transformers": transformers.__version__,
            "torch": torch.__version__,
            "pipeline": "modules.json: Transformer, mean Pooling, Normalize",
        },
        "models": {},
    }
    for name, d in zip(REVISIONS, sys.argv[1:3]):
        out["models"][name] = {"revision": REVISIONS[name], **embed(d)}
    json.dump(out, sys.stdout, ensure_ascii=False, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()

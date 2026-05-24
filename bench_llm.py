"""Quick LLM throughput benchmark — direct LM Studio /v1/chat/completions hit.

Measures wall-clock tokens/sec for completion (not counting prompt processing
time separately, since LM Studio does not expose a TTFT split via the
non-streaming endpoint).  Usage: just run this file."""
import json
import time
import urllib.request

URL = "http://127.0.0.1:1234/v1/chat/completions"
MODEL = "gemma-4-e4b-it"

PROMPTS = [
    # short generation
    ("short", "Write one sentence about Taipei.", 60),
    # medium generation
    ("medium",
     "Write a 250-word descriptive paragraph about a futuristic city. "
     "Be specific and concrete.", 350),
    # longer generation (more representative of agent ReAct turns)
    ("long",
     "Explain how a Rust async runtime schedules tasks across threads, "
     "covering wakers, executors, and reactors. Use concrete examples.",
     600),
]


def call(prompt: str, max_tokens: int):
    body = json.dumps({
        "model": MODEL,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0.7,
        "stream": False,
    }).encode("utf-8")
    req = urllib.request.Request(URL, data=body,
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=300) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    t1 = time.perf_counter()
    return t1 - t0, data["usage"]


def main():
    print(f"# LLM bench — {MODEL} (LM Studio @ {URL})")
    print(f"# warmup → 1 throwaway call ignored")

    # Warmup so KV cache prefill doesn't pollute the first measurement.
    call("warmup", 8)

    results = []
    for label, prompt, max_tok in PROMPTS:
        rows = []
        for _ in range(3):
            elapsed, usage = call(prompt, max_tok)
            comp = usage["completion_tokens"]
            prom = usage["prompt_tokens"]
            tps = comp / elapsed if elapsed else 0.0
            rows.append((elapsed, prom, comp, tps))
        # report median
        rows.sort(key=lambda r: r[0])
        med = rows[len(rows) // 2]
        results.append((label, med, rows))
        print(f"\n## {label} prompt")
        for i, (e, p, c, t) in enumerate(rows, 1):
            print(f"  run {i}: elapsed={e:.2f}s  prompt={p:>4}  completion={c:>4}  throughput={t:.1f} tok/s")
        print(f"  median: {med[3]:.1f} tok/s  ({med[2]} tokens / {med[0]:.2f}s)")

    overall = sum(r[1][3] for r in results) / len(results)
    print(f"\n# overall median across prompts: {overall:.1f} tok/s")


if __name__ == "__main__":
    main()

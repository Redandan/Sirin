import fs from "node:fs";
import path from "node:path";

const cwd = process.cwd();
const casesPath = path.join(cwd, "data", "eval", "cases.jsonl");
const predsPath = path.join(cwd, "data", "eval", "predictions.jsonl");
const outPath = path.join(cwd, "data", "eval", "latest-report.json");

function readJsonl(filePath) {
  if (!fs.existsSync(filePath)) return [];
  return fs
    .readFileSync(filePath, "utf8")
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => JSON.parse(l));
}

function keywordScore(output, expectedKeywords) {
  const text = (output || "").toLowerCase();
  if (!expectedKeywords || expectedKeywords.length === 0) return 1;

  const hit = expectedKeywords.filter((k) => text.includes(String(k).toLowerCase())).length;
  return hit / expectedKeywords.length;
}

function evaluate(cases, preds) {
  const predMap = new Map(preds.map((p) => [p.case_id, p]));

  let weightedTotal = 0;
  let weightedScore = 0;
  let latencyPass = 0;
  let totalLatencyChecked = 0;
  let missingPredictions = 0;

  const details = cases.map((c) => {
    const p = predMap.get(c.case_id);
    const weight = Number(c.weight || 1);

    if (!p) {
      missingPredictions += 1;
      weightedTotal += weight;
      return {
        case_id: c.case_id,
        score: 0,
        weight,
        has_prediction: false,
        latency_pass: null,
      };
    }

    const score = keywordScore(p.output, c.expected_keywords || []);
    weightedTotal += weight;
    weightedScore += score * weight;

    let latencyPassForCase = null;
    if (typeof c.max_latency_ms === "number") {
      totalLatencyChecked += 1;
      latencyPassForCase = Number(p.latency_ms || Number.MAX_SAFE_INTEGER) <= c.max_latency_ms;
      if (latencyPassForCase) latencyPass += 1;
    }

    return {
      case_id: c.case_id,
      score,
      weight,
      has_prediction: true,
      latency_pass: latencyPassForCase,
    };
  });

  const accuracy = weightedTotal > 0 ? weightedScore / weightedTotal : 0;
  const latencyPassRate = totalLatencyChecked > 0 ? latencyPass / totalLatencyChecked : null;

  return {
    generated_at: new Date().toISOString(),
    case_count: cases.length,
    prediction_count: preds.length,
    missing_predictions: missingPredictions,
    weighted_accuracy: Number(accuracy.toFixed(4)),
    latency_pass_rate: latencyPassRate == null ? null : Number(latencyPassRate.toFixed(4)),
    pass_gate: accuracy >= 0.75 && (latencyPassRate == null || latencyPassRate >= 0.8),
    details,
  };
}

const cases = readJsonl(casesPath);
const preds = readJsonl(predsPath);

if (cases.length === 0) {
  console.error("No eval cases found at data/eval/cases.jsonl");
  process.exit(1);
}

const report = evaluate(cases, preds);
fs.writeFileSync(outPath, JSON.stringify(report, null, 2), "utf8");

console.log("Local eval finished");
console.log(`Cases: ${report.case_count}`);
console.log(`Predictions: ${report.prediction_count}`);
console.log(`Weighted accuracy: ${report.weighted_accuracy}`);
console.log(`Latency pass rate: ${report.latency_pass_rate}`);
console.log(`Gate pass: ${report.pass_gate}`);
console.log(`Report: ${outPath}`);

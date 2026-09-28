const SECRET_PATTERNS = [
  /\bBearer\s+[A-Za-z0-9._~+\/-]{12,}={0,2}\b/gi,
  /\b(?:sk|rk)-(?:live|proj)-[A-Za-z0-9_-]{12,}\b/g,
  /\bgh[pousr]_[A-Za-z0-9]{20,}\b/g,
  /\bgithub_pat_[A-Za-z0-9_]{20,}\b/g,
  /\bxox[baprs]-[A-Za-z0-9-]{16,}\b/g,
  /\bAKIA[0-9A-Z]{16}\b/g,
  /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b/g,
  /\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|passwd|secret)\s*[:=]\s*["']?[A-Za-z0-9_./+=-]{8,}["']?/gi,
];

const REPOSITORY_HEADER = /^(?:For this repository,[ \t]+the[ \t]+repository-level policy[ \t]+(is|has changed):|For this repository,[ \t]+repository-level policy[ \t]+(is|has changed):|Repository-wide[ \t]+(?:policy|constraint|rule):)[ \t]*/i;
const OPEN_QUESTION_MARKER = /^Open question:[ \t]*/i;
const REPOSITORY_PREDECESSOR_MARKER = /^Previous repository-level policy to replace:[ \t]*/i;
const SENTENCE_END = /[.!?。！？](?=\s|$)/g;
const CONSTRAINT_DIRECTIVE = /\b(?:must|should|never|always|do not|don't|keep|changed from|replace|instead of|no longer|from now on)\b|(?:必ず|禁止|してはなら|しないこと|維持する|制約|変更|置き換え|今後は)/i;
const DECISION_DIRECTIVE = /\bdecision:\s*|\b(?:decided to|we chose|the decision is|selected because)\b|(?:決定事項|決定した|選択した)/i;
const SUBJECT_SPLIT = /\b(?:must(?:\s+not)?|should|never|always|do not|don't|keep|changed from|replace|instead of|no longer|from now on)\b|(?:必ず|禁止|してはなら|しないこと|維持する|変更|置き換え|今後は)/i;
const SUBJECT_STOPWORDS = new Set([
  "a", "an", "the", "for", "this", "repository", "repo", "policy", "constraint", "rule",
  "must", "not", "be", "is", "are", "do", "does", "should", "always", "never", "changed",
  "change", "from", "to", "instead", "of", "replace", "with", "use", "keep", "now", "on",
]);

export function redactUntrustedText(value) {
  if (typeof value !== "string") return "";
  let output = value;
  for (const pattern of SECRET_PATTERNS) output = output.replace(pattern, "[REDACTED]");
  return output;
}

export function permittedRepositoryClauses(source) {
  if (source?.kind !== "instruction" || typeof source.content_preview !== "string") return [];
  return parseRepositoryDeclarationBlock(source.content_preview).clauses;
}

export function explicitRepositoryPredecessors(source) {
  if (source?.kind !== "instruction" || typeof source.content_preview !== "string") return [];
  return parseRepositoryDeclarationBlock(source.content_preview).predecessors;
}

export function isExplicitRepositoryPredecessor(source, quote) {
  return typeof quote === "string" && explicitRepositoryPredecessors(source).includes(quote.trim());
}

export function repositoryPolicyClause(source, quote) {
  if (typeof quote !== "string") return null;
  return permittedRepositoryClauses(source).find((clause) => clause.quote === quote.trim()) ?? null;
}

export function isExplicitRepositoryPolicy(source, quote) {
  return repositoryPolicyClause(source, quote)?.kind === "constraint";
}

export function isExplicitRepositoryPolicyChange(source, quote, semanticKey) {
  const clause = repositoryPolicyClause(source, quote);
  return clause?.kind === "constraint" && clause.changed
    && semanticKey === `constraint:${clause.subject}`;
}

export function isConstraintQuote(source, quote) {
  if (source?.kind !== "instruction" || typeof quote !== "string") return false;
  const repositoryClause = repositoryPolicyClause(source, quote)?.kind === "constraint";
  const exactSentence = splitSentences(source.content_preview ?? "").includes(quote.trim());
  return repositoryClause || (exactSentence && CONSTRAINT_DIRECTIVE.test(quote));
}

export function isDecisionQuote(source, quote) {
  if (source?.kind !== "instruction" || typeof quote !== "string") return false;
  return splitSentences(source.content_preview ?? "").includes(quote.trim())
    && DECISION_DIRECTIVE.test(quote);
}

export function isUnresolvedQuote(source, quote) {
  if (source?.kind !== "instruction" || typeof quote !== "string") return false;
  if (repositoryPolicyClause(source, quote)?.kind === "unresolved") return true;
  return splitSentences(source.content_preview ?? "").includes(quote.trim())
    && /\b(?:open question|unresolved|unknown|undecided|unclear|not yet decided)\b|(?:未解決|未確定|不明)/i.test(quote);
}

export function isExplicitChangeDirective(source, quote, semanticKey) {
  return isExplicitRepositoryPolicyChange(source, quote, semanticKey);
}

export function deriveScope(source, quote) {
  if (!source || typeof source !== "object") return null;
  const repositoryKey = source.repository_key ?? source.repositoryKey;
  const clause = repositoryPolicyClause(source, quote);
  if (repositoryKey && clause && ["constraint", "unresolved"].includes(clause.kind)) {
    return { type: "repository", id: repositoryKey };
  }
  if (source.task_id) return { type: "task", id: source.task_id };
  if (source.workspace_id) return { type: "workspace", id: source.workspace_id };
  if (source.execution_id) return { type: "execution", id: source.execution_id };
  if (source.operation_id) return { type: "execution", id: "operation:" + source.operation_id };
  return null;
}

export function semanticKeyFor(kind, text) {
  const value = kind === "constraint" || kind === "decision"
    ? semanticSubject(kind, text)
    : normalizedSubject(text);
  return kind + ":" + boundedSemanticSubject(value || "unresolved");
}

function parseRepositoryDeclarationBlock(text) {
  const clauses = [];
  const predecessors = [];
  if (typeof text !== "string") return { clauses, predecessors };
  let cursor = skipWhitespace(text, 0);
  let parsedHeader = false;
  while (cursor < text.length) {
    const header = REPOSITORY_HEADER.exec(text.slice(cursor));
    if (!header) break;
    parsedHeader = true;
    const isRepositoryPrefixed = /^For this repository,/i.test(header[0]);
    const changeWord = header[1] ?? header[2] ?? null;
    cursor = skipOneLineSeparator(text, cursor + header[0].length);
    if (startsQuotedOrFenced(text, cursor)) break;
    const claimEnd = sentenceEndOnLine(text, cursor);
    if (claimEnd < 0) break;
    const quote = text.slice(cursor, claimEnd + 1).trim();
    if (!quote || startsQuotedOrFenced(quote, 0) || !CONSTRAINT_DIRECTIVE.test(quote)) break;
    clauses.push({
      kind: "constraint",
      quote,
      changed: isRepositoryPrefixed && changeWord?.toLowerCase() === "has changed",
      subject: semanticSubject("constraint", quote),
    });
    cursor = skipOneLineSeparator(text, claimEnd + 1);
    const questionMarker = OPEN_QUESTION_MARKER.exec(text.slice(cursor));
    if (questionMarker) {
      cursor += questionMarker[0].length;
      if (startsQuotedOrFenced(text, cursor)) break;
      const questionEnd = sentenceEndOnLine(text, cursor);
      if (questionEnd < 0) break;
      const question = text.slice(cursor, questionEnd + 1).trim();
      if (!question || startsQuotedOrFenced(question, 0)) break;
      clauses.push({
        kind: "unresolved",
        quote: question,
        changed: false,
        subject: semanticSubject("unresolved", question),
      });
      cursor = skipOneLineSeparator(text, questionEnd + 1);
    }
    const predecessorMarker = REPOSITORY_PREDECESSOR_MARKER.exec(text.slice(cursor));
    if (predecessorMarker) {
      cursor += predecessorMarker[0].length;
      if (startsQuotedOrFenced(text, cursor)) break;
      const predecessorEnd = sentenceEndOnLine(text, cursor);
      if (predecessorEnd < 0) break;
      const predecessor = text.slice(cursor, predecessorEnd + 1).trim();
      if (!predecessor || startsQuotedOrFenced(predecessor, 0)) break;
      predecessors.push(predecessor);
      cursor = skipOneLineSeparator(text, predecessorEnd + 1);
    }
    const nextDeclaration = skipWhitespace(text, cursor);
    if (!REPOSITORY_HEADER.test(text.slice(nextDeclaration))) break;
    cursor = nextDeclaration;
  }
  if (!parsedHeader) return { clauses: [], predecessors: [] };
  const deduped = new Map();
  for (const clause of clauses) deduped.set(JSON.stringify([clause.kind, clause.quote]), clause);
  return { clauses: [...deduped.values()], predecessors: [...new Set(predecessors)] };
}

function skipWhitespace(text, start) {
  while (/\s/.test(text[start] ?? "")) start += 1;
  return start;
}

function skipOneLineSeparator(text, start) {
  let cursor = start;
  while (text[cursor] === " " || text[cursor] === "\t") cursor += 1;
  if (text[cursor] === "\r" && text[cursor + 1] === "\n") cursor += 2;
  else if (text[cursor] === "\r" || text[cursor] === "\n") cursor += 1;
  while (text[cursor] === " " || text[cursor] === "\t") cursor += 1;
  return cursor;
}

function sentenceEndOnLine(text, start) {
  const end = sentenceEnd(text, start);
  const newline = text.indexOf("\n", start);
  return end >= 0 && (newline < 0 || end < newline) ? end : -1;
}

function startsQuotedOrFenced(text, start) {
  const first = text.slice(start);
  const backtick = String.fromCharCode(96);
  return first.startsWith(">")
    || first.startsWith(backtick.repeat(3))
    || first.startsWith("~~~")
    || first.startsWith("\"")
    || first.startsWith(String.fromCharCode(39))
    || first.startsWith(backtick);
}

function semanticSubject(kind, text) {
  const value = text.trim().replace(/^["'`]+|["'`.!?。！？]+$/g, "");
  const match = (kind === "constraint" ? SUBJECT_SPLIT : DECISION_DIRECTIVE).exec(value);
  const prefix = match ? value.slice(0, match.index) : value;
  return normalizedSubject(prefix || value);
}

function normalizedSubject(value) {
  const normalized = value.normalize("NFKC").toLowerCase();
  const words = normalized.match(/[\p{L}\p{N}]+/gu) ?? [];
  const subject = words.filter((word) => !SUBJECT_STOPWORDS.has(word)).join("-");
  if (!subject && !words.length) return unicodeSubjectHash(normalized);
  // Preserve the complete subject when Japanese or another non-ASCII script
  // is mixed with Latin identifiers. ASCII-only tokenization used to collapse
  // unrelated subjects such as "出力 API" and "内部 API" to the same "api" key.
  if ([...normalized].some((character) => character.codePointAt(0) > 0x7f)) {
    return unicodeSubjectHash(subject || words.join("-"));
  }
  return subject || words.join("-");
}

function boundedSemanticSubject(subject) {
  if (new TextEncoder().encode(subject).byteLength <= 96) return subject;
  return subject.slice(0, 64) + "-h" + unicodeSubjectHash(subject).slice(2);
}

function unicodeSubjectHash(value) {
  let hash = 14695981039346656037n;
  for (const byte of new TextEncoder().encode(value.normalize("NFKC").toLowerCase())) {
    hash ^= BigInt(byte);
    hash = (hash * 1099511628211n) & 0xffffffffffffffffn;
  }
  return "u-" + hash.toString(16);
}

function splitSentences(text) {
  if (typeof text !== "string") return [];
  const output = [];
  let start = 0;
  SENTENCE_END.lastIndex = 0;
  let match;
  while ((match = SENTENCE_END.exec(text)) !== null) {
    output.push(text.slice(start, match.index + 1).trim());
    start = match.index + 1;
  }
  if (text.slice(start).trim()) output.push(text.slice(start).trim());
  return output;
}

function sentenceEnd(text, start) {
  SENTENCE_END.lastIndex = start;
  const match = SENTENCE_END.exec(text);
  return match?.index ?? -1;
}

export function hasPotentialSecret(value) {
  return typeof value === "string" && redactUntrustedText(value) !== value;
}

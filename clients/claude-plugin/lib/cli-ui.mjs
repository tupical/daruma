const isTTY = Boolean(process.stdout.isTTY);
const colorEnabled = isTTY && !("NO_COLOR" in process.env);

const code = (n) => colorEnabled ? `\u001b[${n}m` : "";

const c = {
  bold: code(1),
  dim: code(2),
  blue: code(34),
  cyan: code(36),
  green: code(32),
  yellow: code(33),
  red: code(31),
  reset: code(0),
};

const symbols = isTTY
  ? { step: "▸", ok: "✓", warn: "!", fail: "✗", dot: "•" }
  : { step: "->", ok: "OK", warn: "WARN", fail: "FAIL", dot: "-" };

const frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

function line(text = "") {
  process.stdout.write(`${text}\n`);
}

function clearLine() {
  if (isTTY) process.stdout.write("\r\u001b[2K");
}

export function createCliUi({ title } = {}) {
  function header(text = title) {
    line("");
    line(`${c.bold}${text}${c.reset}`);
    line("");
  }

  function section(text) {
    line("");
    line(`${c.bold}${text}${c.reset}`);
  }

  function step(text) {
    line(`${c.blue}${symbols.step}${c.reset} ${text}`);
  }

  function success(text) {
    line(`${c.green}${symbols.ok}${c.reset} ${text}`);
  }

  function warn(text) {
    line(`${c.yellow}${symbols.warn}${c.reset} ${text}`);
  }

  function error(text) {
    line(`${c.red}${symbols.fail}${c.reset} ${text}`);
  }

  function detail(text) {
    line(`${c.dim}${text}${c.reset}`);
  }

  function item(text, { kind = "dot" } = {}) {
    const palette = {
      ok: c.green,
      warn: c.yellow,
      fail: c.red,
      dot: c.cyan,
    };
    const symbol = symbols[kind] ?? symbols.dot;
    line(`  ${palette[kind] ?? c.cyan}${symbol}${c.reset} ${text}`);
  }

  async function task(label, fn, doneLabel = label) {
    if (!isTTY) {
      step(label);
      try {
        const result = await fn();
        success(doneLabel);
        return result;
      } catch (err) {
        error(label);
        throw err;
      }
    }

    let i = 0;
    process.stdout.write(`${c.blue}${frames[i]}${c.reset} ${label}`);
    const timer = setInterval(() => {
      i = (i + 1) % frames.length;
      process.stdout.write(`\r${c.blue}${frames[i]}${c.reset} ${label}`);
    }, 80);
    try {
      const result = await fn();
      clearInterval(timer);
      clearLine();
      success(doneLabel);
      return result;
    } catch (err) {
      clearInterval(timer);
      clearLine();
      error(label);
      throw err;
    }
  }

  return { header, section, step, success, warn, error, detail, item, task };
}

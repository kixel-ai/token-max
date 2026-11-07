import stringWidth from "string-width";

const graphemes = new Intl.Segmenter();

/** Wrap plain text by terminal cells without normalizing Unicode or shortening words. */
export function wrapText(
  text: string,
  options: { width?: number; indent?: number } = {},
): string {
  return text
    .split("\n")
    .map((input) => {
      const leading = input.match(/^ */)![0].length;
      const indent = leading + (options.indent ?? 0);
      if (options.width === undefined)
        return " ".repeat(options.indent ?? 0) + input;
      const width = Math.max(1, Math.floor(options.width));
      const padding = " ".repeat(Math.min(indent, Math.max(0, width - 2)));
      const columns = width - padding.length;
      const rows: string[] = [];
      let row = "";
      let cells = 0;
      let spaces = "";
      function flush() {
        rows.push(padding + row);
        row = "";
        cells = 0;
      }
      for (const word of input.slice(leading).match(/ +|[^ ]+/gu) ?? []) {
        if (/^ +$/.test(word)) {
          spaces += word;
          continue;
        }
        if (row && cells + spaces.length + stringWidth(word) > columns) {
          flush();
          spaces = "";
        }
        row += spaces;
        cells += spaces.length;
        spaces = "";
        for (const { segment } of graphemes.segment(word)) {
          const size = stringWidth(segment);
          if (cells && cells + size > columns) flush();
          row += segment;
          cells += size;
        }
      }
      flush();
      return rows.join("\n");
    })
    .join("\n");
}

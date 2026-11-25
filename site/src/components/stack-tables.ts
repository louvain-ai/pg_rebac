// Presentation only: a rendered table of four or more columns of prose is marked `stack`, and each body cell gets its column's
// heading as `data-label`, so theme.css can lay each row out as a labelled block on a narrow screen instead of a
// table that scrolls sideways. Cell content is untouched. It works on the HTML Markdown renders, which writes plain
// <table>, <thead>, <th> and <td> tags. A table of figures (short cells) keeps its grid and scrolls instead: its
// numbers are read across a row and down a column, which a stack would break apart.
const strip = (html: string) => html.replace(/<[^>]*>/g, "").replace(/"/g, "&quot;").trim();

export function stackTables(html: string): string {
  return html.replace(/<table>([\s\S]*?)<\/table>/g, (whole, inner: string) => {
    const head = /<thead>([\s\S]*?)<\/thead>/.exec(inner);
    const labels = head ? [...head[1].matchAll(/<th[^>]*>([\s\S]*?)<\/th>/g)].map((m) => strip(m[1])) : [];
    if (labels.length < 4) return whole;
    const cells = [...inner.matchAll(/<td[^>]*>([\s\S]*?)<\/td>/g)].map((m) => strip(m[1]).length);
    if (!cells.length || cells.reduce((a, b) => a + b, 0) / cells.length < 24) return whole;
    const body = inner.replace(/<tr>([\s\S]*?)<\/tr>/g, (row, cells: string) => {
      if (/<th[\s>]/.test(cells)) return row;
      let i = 0;
      return `<tr>${cells.replace(/<td(\s[^>]*)?>/g, (_m, attrs = "") => `<td${attrs} data-label="${labels[i++] ?? ""}">`)}</tr>`;
    });
    return `<table class="stack">${body}</table>`;
  });
}

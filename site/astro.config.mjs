// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

// Only the production deployment (pgrebac.com, attached at launch) may be indexed. Every preview, and any build
// outside Vercel, says noindex.
const indexable = process.env.VERCEL_ENV === "production";
const robots = indexable ? [] : [{ tag: "meta", attrs: { name: "robots", content: "noindex, nofollow" } }];
const og = "https://pgrebac.com/og.png";
const head = [
  ...robots,
  { tag: "link", attrs: { rel: "icon", href: "/favicon.ico", sizes: "32x32" } },
  { tag: "link", attrs: { rel: "apple-touch-icon", href: "/apple-touch-icon.png" } },
  { tag: "meta", attrs: { property: "og:image", content: og } },
  { tag: "meta", attrs: { property: "og:image:width", content: "1200" } },
  { tag: "meta", attrs: { property: "og:image:height", content: "630" } },
  { tag: "meta", attrs: { name: "twitter:image", content: og } },
];

export default defineConfig({
  site: "https://pgrebac.com",
  redirects: { "/docs": "/docs/getting-started/" },
  integrations: [
    starlight({
      title: "pg_rebac",
      description:
        "Permission-aware reads inside Postgres: SpiceDB relationships mirrored into an indexed, planner-friendly readable set.",
      logo: { light: "./src/assets/mark-light.svg", dark: "./src/assets/mark-dark.svg" },
      favicon: "/favicon.svg",
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/louvain-ai/pg_rebac" }],
      customCss: [
        "@fontsource-variable/instrument-sans",
        "@fontsource-variable/jetbrains-mono",
        "./src/styles/theme.css",
      ],
      head,
      // Code frames in the cover's panel, line and radius; Expressive Code's copy button is kept.
      expressiveCode: {
        themes: ["github-dark-default", "github-light-default"],
        useStarlightUiThemeColors: true,
        // A plain frame for shell too, without the terminal window's dots.
        defaultProps: { frame: "code", overridesByLang: { "sh,bash,shell,console,zsh": { frame: "code" } } },
        styleOverrides: {
          borderRadius: "6px",
          borderColor: "var(--sl-color-hairline)",
          codeFontFamily: "var(--sl-font-mono)",
          codeFontSize: "0.8125rem",
          codeLineHeight: "1.6",
          uiFontFamily: "var(--sl-font)",
          frames: { frameBoxShadowCssValue: "none", inlineButtonBorderOpacity: "0.5" },
        },
      },
      components: {
        Footer: "./src/components/Footer.astro",
        MarkdownContent: "./src/components/MarkdownContent.astro",
      },
      lastUpdated: false,
      pagination: true,
      sidebar: [
        { label: "Getting started", slug: "docs/getting-started" },
        {
          label: "Concepts",
          items: [
            { label: "The readable set", slug: "docs/concepts/readable-set" },
            { label: "Breadth and the two paths", slug: "docs/concepts/breadth" },
            { label: "Revocations and consistency", slug: "docs/concepts/consistency" },
          ],
        },
        { label: "rebacd", slug: "docs/rebacd" },
        { label: "SpiceDB schema support", slug: "docs/schema-support" },
        { label: "Performance", slug: "docs/performance" },
        { label: "Operations", slug: "docs/operations" },
        { label: "Why not X?", slug: "docs/why-not" },
        { label: "FAQ", slug: "docs/faq" },
      ],
    }),
  ],
});

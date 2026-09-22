import { defineConfig } from "vitepress";

// https://vitepress.dev/reference/site-config
export default defineConfig({
  title: "favetto",
  description:
    "A long-running, LLM-driven agent orchestrator with a remote-TUI-first design.",
  base: "/favetto/",
  cleanUrls: true,
  lastUpdated: true,
  themeConfig: {
    nav: [
      { text: "Guide", link: "/guide/getting-started" },
      { text: "Reference", link: "/reference/architecture" },
      { text: "GitHub", link: "https://github.com/oknozor/favetto" },
    ],
    sidebar: {
      "/guide/": [
        {
          text: "Guide",
          items: [
            { text: "Getting started", link: "/guide/getting-started" },
            { text: "Tasks & catalog", link: "/guide/tasks" },
            { text: "Embedded agents", link: "/guide/agents" },
            { text: "Configuration", link: "/guide/configuration" },
            { text: "Webhooks & hooks", link: "/guide/webhooks" },
            { text: "TUI", link: "/guide/tui" },
            { text: "Execution & worktrees", link: "/guide/execution" },
          ],
        },
      ],
      "/reference/": [
        {
          text: "Reference",
          items: [
            { text: "Architecture", link: "/reference/architecture" },
            { text: "Remote API", link: "/reference/remote-api" },
          ],
        },
      ],
    },
    search: {
      provider: "local",
    },
    editLink: {
      pattern: "https://github.com/oknozor/favetto/edit/main/docs/:path",
      text: "Edit this page on GitHub",
    },
    socialLinks: [
      { icon: "github", link: "https://github.com/oknozor/favetto" },
    ],
    footer: {
      message: "Released under the MIT License.",
      copyright: "Copyright © oknozor",
    },
  },
  vite: {
    server: {
      fs: {
        // `configuration.md` includes ../../config.example.toml, which lives
        // outside the docs root.
        allow: [".."],
      },
    },
  },
});

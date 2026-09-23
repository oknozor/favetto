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
      { text: "Guide", link: "/guide/installation" },
      { text: "Reference", link: "/reference/architecture" },
      { text: "GitHub", link: "https://github.com/oknozor/favetto" },
    ],
    sidebar: {
      "/guide/": [
        {
          text: "Introduction",
          items: [
            { text: "Installation", link: "/guide/installation" },
            { text: "Getting started", link: "/guide/getting-started" },
            { text: "Your first task", link: "/guide/first-task" },
          ],
        },
        {
          text: "Guides",
          items: [
            { text: "Catalog", link: "/guide/catalog" },
            { text: "Variables & prompts", link: "/guide/variables-and-prompts" },
            { text: "Embedded agents", link: "/guide/agents" },
            {
              text: "Schedules & dependencies",
              link: "/guide/schedules-and-dependencies",
            },
            { text: "Webhooks & hooks", link: "/guide/webhooks" },
            { text: "Parallel execution", link: "/guide/parallel-worktrees" },
            { text: "Git & signing", link: "/guide/git-signing" },
            { text: "Remote access", link: "/guide/remote-access" },
            { text: "Configuration", link: "/guide/configuration" },
            { text: "TUI", link: "/guide/tui" },
            { text: "Troubleshooting", link: "/guide/troubleshooting" },
          ],
        },
        {
          text: "Contributing",
          items: [
            { text: "Releasing", link: "/guide/releasing" },
          ],
        },
      ],
      "/reference/": [
        {
          text: "Reference",
          items: [
            { text: "CLI", link: "/reference/cli" },
            { text: "Configuration", link: "/reference/config" },
            { text: "Task file format", link: "/reference/tasks" },
            { text: "Event kinds", link: "/reference/events" },
            { text: "Environment", link: "/reference/environment" },
            { text: "Remote API", link: "/reference/remote-api" },
            { text: "Architecture", link: "/reference/architecture" },
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

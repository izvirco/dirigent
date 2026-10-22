import type { WorkflowDef } from "../../dirigent_runtime/src/workflow";

// N comedian agents each tell a joke on a theme, then a judge agent ranks them.
// Themes come from the `themes` input (comma-separated); if empty, each comedian picks its own.
export default {
  description: "Comedian agents each tell a joke, a judge agent ranks them",
  input: {
    count: { type: "number", description: "Number of comedians", default: 3 },
    themes: {
      type: "string",
      description: "Comma-separated themes (blank = each picks a random one)",
      default: "",
    },
    comedianModel: { type: "string", default: "anthropic/claude-haiku-5-5" },
    comedianEffort: { type: "string", default: "medium" },
    judgeModel: { type: "string", default: "anthropic/claude-sonnet-5-5" },
  },
  async run(d, input) {
    const themes = input.themes
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
    const labels = Array.from({ length: input.count }, (_, i) => String.fromCharCode(65 + i));

    const ids = await Promise.all(
      labels.map((_, i) => {
        const theme = themes[i];
        const ask = theme
          ? `on the theme: "${theme}".`
          : "on a random, unusual theme you pick yourself. Start your reply with `Theme: <theme>` on its own line.";
        return d.spawn({
          name: `Comedian ${labels[i]}`,
          task: `Tell one original, short joke ${ask} Reply with only the joke, no tools, no preamble.`,
          model: input.comedianModel,
          effort: input.comedianEffort,
        });
      }),
    );
    await d.wait(ids);
    const jokes = await Promise.all(ids.map(async (id) => (await d.read(id)).reply.trim()));

    const entries = labels
      .map((l, i) => `Agent ${l}${themes[i] ? ` (theme: ${themes[i]})` : ""}:\n${jokes[i]}`)
      .join("\n\n");
    const judge = await d.spawn({
      name: "Judge",
      task:
        "Rate these jokes from 1-10 and rank them best to worst. Be a fair, discerning comedy judge. " +
        "Reply with a ranked list: position, author, score, one-sentence reason. No tools.\n\n" +
        entries,
      model: input.judgeModel,
    });
    await d.wait([judge]);
    const ranking = (await d.read(judge)).reply;

    await d.log(`${entries}\n\n--- Ranking ---\n${ranking}`);
    return { jokes: Object.fromEntries(labels.map((l, i) => [l, jokes[i]])), ranking };
  },
} satisfies WorkflowDef<{
  count: number;
  themes: string;
  comedianModel: string;
  comedianEffort: string;
  judgeModel: string;
}>;

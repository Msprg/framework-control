<script context="module" lang="ts">
    /** How a value, a card or its marker on the laptop photo is coloured. */
    export type Tone = "ok" | "info" | "warn" | "error" | "idle" | "plain";
    /** `hint` replaces the value as the hover text when the value needs explaining. */
    export type Row = { label: string; value: string; tone?: Tone; hint?: string };
    export type Group = { title: string; icon: string; rows: Row[] };

    export const TONE_TEXT: Record<Tone, string> = {
        ok: "text-success",
        info: "text-info",
        warn: "text-warning",
        error: "text-error",
        idle: "text-base-content/40",
        plain: "text-base-content/70",
    };

    export const TONE_BG: Record<Tone, string> = {
        ok: "bg-success",
        info: "bg-info",
        warn: "bg-warning",
        error: "bg-error",
        idle: "bg-base-content/40",
        plain: "bg-base-content/70",
    };
</script>

<script lang="ts">
    import Icon from "@iconify/svelte";

    export let group: Group;
    export let tone: Tone = "plain";
    /** Show the dot that matches this card's marker on the laptop photo. */
    export let dot = false;
</script>

<div class="bg-base-200 rounded-box border surface-border px-3 py-2 min-w-0">
    <div class="flex items-center gap-2 text-xs font-medium mb-1">
        {#if dot}
            <span class="w-2 h-2 rounded-full shrink-0 {TONE_BG[tone]}"></span>
        {/if}
        <Icon icon={group.icon} class="w-4 h-4 shrink-0" />
        <span>{group.title}</span>
    </div>
    <dl class="text-xs space-y-0.5">
        {#each group.rows as row (row.label)}
            <div class="flex justify-between gap-3">
                <dt class="opacity-60 shrink-0">{row.label}</dt>
                <dd
                    class="text-right truncate {row.tone ? TONE_TEXT[row.tone] : ''}"
                    title={row.hint ?? row.value}
                >
                    {row.value}
                </dd>
            </div>
        {/each}
    </dl>
</div>

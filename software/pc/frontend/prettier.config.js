/** @type {import("prettier").Config} */
const config = {
    useTabs: false,
    tabWidth: 4,
    singleQuote: true,
    trailingComma: 'none',
    bracketSameLine: true,
    plugins: ['prettier-plugin-svelte', 'prettier-plugin-organize-imports'],
    overrides: [
        {
            files: '*.svelte',
            options: { parser: 'svelte' }
        }
    ]
};
export default config;

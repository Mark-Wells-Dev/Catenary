// Conformance fixture (tui-rework 10).
// Intentional diagnostic: clangd flags the use of an undeclared identifier — a
// hard parse error it publishes on didOpen through the shipped default config.
int main(void) {
    return undefined_conformance_symbol;
}

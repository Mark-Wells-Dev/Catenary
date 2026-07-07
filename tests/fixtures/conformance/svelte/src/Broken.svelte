<script lang="ts">
  // Conformance fixture (tui-rework 07).
  // Intentional diagnostic: TS2322 in the component script — a string assigned
  // to a number; svelte-language-server surfaces it through its TS plugin.
  const answer: number = "not a number";
</script>

<p>{answer}</p>

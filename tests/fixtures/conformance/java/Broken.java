// Conformance fixture (tui-rework 10).
// Intentional diagnostic: jdtls (Eclipse JDT) flags the type mismatch — a String
// assigned to an int — through the shipped default config. Driven as a
// standalone file (no build.gradle/pom.xml): jdtls's invisible-project mode
// still type-checks and publishes, without a Gradle/Maven import on the runner.
public class Broken {
    public static void main(String[] args) {
        int answer = "not a number";
        System.out.println(answer);
    }
}

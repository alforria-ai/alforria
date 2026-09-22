fn dump(node: tree_sitter::Node, src: &str, depth: usize) {
    println!(
        "{}{}{} [{}]",
        "  ".repeat(depth),
        node.kind(),
        if node.is_error() { " ERROR" } else { "" },
        &src[node.byte_range()],
    );
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        dump(child, src, depth + 1);
    }
}

fn main() {
    let cases = [
        "\"pre\"a\"post\"",
        "ls $HOME/doc",
        "ls ~",
        "(cd /tmp)",
        "{ echo hi; }",
        "echo $(date)",
        "echo `date`",
        "cat <<EOF\nhello\nEOF",
        "echo hi >out.txt 2>err.txt",
        "true|false",
        "if [ -f x ]; then echo y; fi",
        "for i in 1 2; do echo $i; done",
        "x=1; echo $x",
        "sudo rm -rf /",
        "bash -c \"echo hi\"",
        "rm -rf $HOME/x || true",
        "cp a b && mkdir -p newdir",
        "git commit -m \"a b c\"",
        "ls 2>/dev/null",
        "exec 3<&0",
    ];
    for code in cases {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .expect("load bash language");
        let tree = parser.parse(code, None).expect("parse");
        println!("=== {code:?}");
        dump(tree.root_node(), code, 0);
    }
}

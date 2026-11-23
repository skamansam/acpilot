module.exports = grammar({
  name: 'simple',

  extras: $ => [/\s/],

  rules: {
    source_file: $ => repeat($._item),
    _item: $ => choice($.word, $.number),
    word: $ => /[a-zA-Z_]+/,
    number: $ => /[0-9]+/,
  },
});

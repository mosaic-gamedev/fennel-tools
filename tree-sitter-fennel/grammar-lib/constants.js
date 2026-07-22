module.exports = {
	// Single-character reader macros handled by the external scanner.
	// NOTE: unquote (,) and unquote_splice (,@) are intentionally absent —
	// the scanner handles them specially because ,@ is a two-character token
	// and Fennel allows commas as whitespace separators (only , immediately
	// followed by a non-whitespace, non-delimiter character is an unquote).
	READER_MACROS: [
		['hashfn',     '#'],
		['quote',      '\''],
		['quasi_quote', '`'],
		['unquote',    ','],
		// unquote_splice is listed here so nodify_reader_macros generates its rule
		// and external token slot, but the scanner handles the two-char ,@ sequence.
		['unquote_splice', ',@'],
	],

	SPECIAL_STANDALONE_SYMBOLS: [
		'#',
		'?.',
		'~=',
		':',
		'$...',
		'...',
		'..',
		'.',
	],
}

# Build the primer PDF.
#
# images/primer-title.pdf is a generated (git-ignored) artifact: rsvg-convert
# turns the committed SVG into a vector PDF that xelatex embeds on the title page.

PRIMER_PDF := PRIMER.pdf
TITLE_PDF  := images/primer-title.pdf
TITLE_SVG  := images/primer-title.svg

.PHONY: primer clean

primer: $(PRIMER_PDF)

$(PRIMER_PDF): PRIMER.md $(TITLE_PDF)
	pandoc PRIMER.md -o $@ --pdf-engine=xelatex

$(TITLE_PDF): $(TITLE_SVG)
	rsvg-convert -f pdf -o $@ $<

clean:
	rm -f $(TITLE_PDF)

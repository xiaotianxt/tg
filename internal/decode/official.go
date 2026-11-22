package decode

import "strings"

// Official-account pushes are subtype-5 app messages whose `<mmreader>`
// always carries `<category>`, `<template_header>` and `<template_detail>`
// elements, most of them empty. A push is a service notice when its template
// yields at least one line, otherwise an article push when its category has
// a titled item.

// decodeOfficialPush renders a notice or article push; false when the
// message has neither, so the caller keeps the plain link display.
func decodeOfficialPush(content string) (string, bool) {
	reader, ok := FindXMLElement(content, "mmreader")
	if !ok {
		return "", false
	}
	mm := reader.Body(content)
	items := articleItems(mm)
	if notice, ok := decodeTemplateNotice(mm, items); ok {
		return notice, true
	}
	if len(items) == 0 {
		return "", false
	}
	return displayArticles(articleSource(mm, items), items), true
}

type articleItem struct {
	title, digest, url, source string
}

// articleItems are the titled `<item>`s of the reader's `<category>`.
func articleItems(mm string) []articleItem {
	category, ok := FindXMLElement(mm, "category")
	if !ok {
		return nil
	}
	rest := category.Body(mm)
	var items []articleItem
	for {
		e, ok := FindXMLElement(rest, "item")
		if !ok {
			return items
		}
		itemXML := e.Body(rest)
		rest = rest[e.End:]
		title, ok := cdataField(itemXML, "title")
		if !ok {
			continue
		}
		item := articleItem{title: title}
		item.digest, _ = cdataField(itemXML, "digest")
		item.url, _ = cdataURL(itemXML, "url")
		if item.url == "" {
			item.url, _ = cdataURL(itemXML, "shorturl")
		}
		if sources, ok := FindXMLElement(itemXML, "sources"); ok {
			item.source, _ = cdataField(sources.Body(itemXML), "name")
		}
		items = append(items, item)
	}
}

// articleSource is the publishing account's name: the reader's publisher,
// else the first item's source; "" when neither is set.
func articleSource(mm string, items []articleItem) string {
	if publisher, ok := FindXMLElement(mm, "publisher"); ok {
		if name, ok := cdataField(publisher.Body(mm), "nickname"); ok {
			return name
		}
	}
	return items[0].source
}

func displayArticles(source string, items []articleItem) string {
	var out strings.Builder
	out.WriteString("[文章]")
	if source != "" {
		out.WriteString(" " + source)
	}
	for _, item := range items {
		out.WriteString("\n  · " + item.title)
		if len(items) == 1 && item.digest != "" {
			out.WriteString(" - " + item.digest)
		}
		if item.url != "" {
			out.WriteString("\n    " + item.url)
		}
	}
	return out.String()
}

// decodeTemplateNotice renders `[通知] <title>` and one line per template
// row: the `<topline>` and the `<lines>` of `<line_content>` and
// `<flat_content>`, in that order.
func decodeTemplateNotice(mm string, items []articleItem) (string, bool) {
	detail, ok := FindXMLElement(mm, "template_detail")
	if !ok {
		return "", false
	}
	body := detail.Body(mm)
	var lines []string
	if lc, ok := FindXMLElement(body, "line_content"); ok {
		lcBody := lc.Body(body)
		if top, ok := FindXMLElement(lcBody, "topline"); ok {
			lines = appendTemplateLine(lines, top.Body(lcBody))
		}
		lines = appendTemplateLines(lines, lcBody)
	}
	if fc, ok := FindXMLElement(body, "flat_content"); ok {
		lines = appendTemplateLines(lines, fc.Body(body))
	}
	if len(lines) == 0 {
		return "", false
	}

	title := ""
	if header, ok := FindXMLElement(mm, "template_header"); ok {
		title, _ = cdataField(header.Body(mm), "title")
	}
	if title == "" && len(items) > 0 {
		title = items[0].title
	}
	out := "[通知]"
	if title != "" {
		out += " " + title
	}
	return out + "\n  " + strings.Join(lines, "\n  "), true
}

func appendTemplateLines(lines []string, container string) []string {
	list, ok := FindXMLElement(container, "lines")
	if !ok {
		return lines
	}
	rest := list.Body(container)
	for {
		e, ok := FindXMLElement(rest, "line")
		if !ok {
			return lines
		}
		lines = appendTemplateLine(lines, e.Body(rest))
		rest = rest[e.End:]
	}
}

// appendTemplateLine adds `<key>：<value>` for one row. A key marked
// `<hide>1</hide>` is not shown by the client, so only its value is kept.
func appendTemplateLine(lines []string, row string) []string {
	var key, value string
	if k, ok := FindXMLElement(row, "key"); ok {
		kBody := k.Body(row)
		if hide, _ := cdataField(kBody, "hide"); hide != "1" {
			key, _ = cdataField(kBody, "word")
			key = strings.TrimRight(key, "：:")
		}
	}
	if v, ok := FindXMLElement(row, "value"); ok {
		value, _ = cdataField(v.Body(row), "word")
	}
	switch {
	case key != "" && value != "":
		return append(lines, key+"："+value)
	case key != "":
		return append(lines, key)
	case value != "":
		return append(lines, value)
	}
	return lines
}

// cdataField is a tag's text without its CDATA wrapper, under the same
// rules as cleanSummaryField.
func cdataField(xml, tag string) (string, bool) {
	v, ok := ExtractXMLTag(xml, tag)
	if !ok {
		return "", false
	}
	return cleanSummaryField(StripCDATA(v))
}

func cdataURL(xml, tag string) (string, bool) {
	v, ok := ExtractXMLTag(xml, tag)
	if !ok {
		return "", false
	}
	return cleanURLField(StripCDATA(v))
}
